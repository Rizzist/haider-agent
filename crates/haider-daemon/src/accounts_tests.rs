//! Unit tests for the daemon account seam: physical-alias namespacing,
//! staged-secret laws, the login receipt shape, and the R10 startup
//! reconciliation over every crash boundary.
#![allow(clippy::expect_used)]

use super::*;
use haider_core::SqliteStoreHandle;

use crate::provider_registry::ProviderProfileV1;
use crate::session_hub::FrameSendError;
use crate::worker::ProviderFactory as _;
use haider_core::ProviderAttemptResolver as _;
use haider_rpc::{ModelDetailWire, ProviderApiFamilyWire, ProviderAuthRequirementWire};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::oauth::{
    OAuthProviderRegistration, OAuthPublicError, OAuthRefreshExchange, OAuthTokenRequestEncoding,
    oauth_import_read_count, reset_oauth_import_read_count,
};

fn test_store_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("hacct")
        .tempdir_in("/tmp")
        .unwrap_or_else(|error| panic!("tempdir: {error}"))
}

fn memory_accounts() -> AccountStore<Box<dyn StoreLike>> {
    struct Ephemeral(StdMutex<Vec<CredentialDescriptor>>);
    impl StoreLike for Ephemeral {
        fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
            Ok(self.0.lock().map(|view| view.clone()).unwrap_or_default())
        }
        fn save(&self, descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
            if let Ok(mut view) = self.0.lock() {
                *view = descriptors.to_vec();
            }
            Ok(())
        }
    }
    let store: Box<dyn StoreLike> = Box::new(Ephemeral(StdMutex::new(Vec::new())));
    AccountStore::new(store).unwrap_or_else(|error| panic!("accounts: {error:?}"))
}

#[derive(Default)]
struct TestProviderStore(StdMutex<Vec<ProviderProfileV1>>);

impl ProviderRegistryStoreLike for TestProviderStore {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError> {
        Ok(self.0.lock().map(|view| view.clone()).unwrap_or_default())
    }

    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        if let Ok(mut view) = self.0.lock() {
            *view = profiles.to_vec();
        }
        Ok(())
    }
}

/// A fresh registry for the direct `handle_login` harness calls (G4b: the
/// login handler consults the registry for the full provider publish).
fn login_registry() -> ProviderRegistry<Box<dyn ProviderRegistryStoreLike>> {
    test_provider_registry()
}

/// A gcloud source no test may reach: any invocation is a loud failure so a
/// mis-scripted test can never shell out to a real `gcloud`.
struct UnreachableGcloud;

impl crate::gcloud::GcloudAccessTokenSource for UnreachableGcloud {
    fn print_access_token(&self) -> Result<zeroize::Zeroizing<Vec<u8>>, HaiderError> {
        Err(HaiderError::new(
            ErrorCode::Internal,
            "test gcloud source must not be invoked",
            false,
        ))
    }
}

fn test_provider_registry() -> ProviderRegistry<Box<dyn ProviderRegistryStoreLike>> {
    let store: Box<dyn ProviderRegistryStoreLike> = Box::new(TestProviderStore::default());
    let model_source = Arc::new(CachedProviderModelSource::default());
    for provider in [
        ANTHROPIC_PROVIDER_NAME,
        ANTHROPIC_OAUTH_PROVIDER_NAME,
        OPENAI_PROVIDER_NAME,
        OPENAI_OAUTH_PROVIDER_NAME,
        "custom",
    ] {
        model_source.replace(
            provider.to_owned(),
            ["claude-test", "gpt-test", "model-a", "model-b", "unused"]
                .into_iter()
                .map(|slug| haider_provider::DiscoveredModel {
                    slug: slug.to_owned(),
                    display_name: format!("Fixture {slug}"),
                    context_window: None,
                    description: Some("provider-owned test fixture".to_owned()),
                    default_effort: None,
                    supported_efforts: Vec::new(),
                    visible: true,
                    priority: None,
                    extensions: None,
                })
                .collect(),
        );
    }
    ProviderRegistry::new(
        store,
        initial_provider_profiles(
            &std::collections::BTreeSet::from([
                ANTHROPIC_PROVIDER_NAME.to_owned(),
                ANTHROPIC_OAUTH_PROVIDER_NAME.to_owned(),
                OPENAI_PROVIDER_NAME.to_owned(),
                OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
            ]),
            "claude-test",
        ),
        model_source,
    )
    .expect("provider registry")
}

fn identity_for(profile: &str, command: &str) -> LoginIdentity {
    LoginIdentity {
        provider: "anthropic".into(),
        resolved_model: "claude-test".into(),
        display_alias: Some("work".into()),
        physical_alias: physical_alias(profile, "anthropic", command),
    }
}

fn oauth_identity_for(profile: &str, command: &str) -> OAuthAddIdentity {
    OAuthAddIdentity {
        provider: "fake-oauth".into(),
        display_alias: "work-oauth".into(),
        physical_alias: physical_alias(profile, "fake-oauth", command),
        auth_method: "oauth".into(),
    }
}

fn oauth_bundle() -> haider_accounts::OAuthTokenBundleV1 {
    haider_accounts::OAuthTokenBundleV1::new(
        "fake-oauth".into(),
        "http://127.0.0.1:32109".into(),
        "fake-audience".into(),
        Some("fake-api-resource".into()),
        "Bearer".into(),
        Zeroizing::new(b"OAUTH_ACCESS_RECEIPT_SENTINEL_1fa2".to_vec()),
        Some(Zeroizing::new(
            b"OAUTH_REFRESH_RECEIPT_SENTINEL_9c31".to_vec(),
        )),
        u64::MAX - 1,
        Some(u64::MAX),
        vec!["openid".into(), "inference".into()],
        haider_accounts::OAuthIdentityV1 {
            subject_hash: "verified-subject".into(),
            display_identity: "person@example.invalid".into(),
        },
        1,
    )
    .expect("OAuth bundle")
}

// MUTATION CHECK (W5a/B6a provider dispatch): remove either native API-key
// arm from `build_account_provider`. Expected failure: OpenAI or Gemini
// resolution returns InvalidArgument before capabilities identify the native
// adapter.
#[tokio::test]
async fn production_account_factory_dispatches_native_api_key_providers() {
    let validator = ProviderCredentialValidator;
    assert!(validator.supports(ANTHROPIC_PROVIDER_NAME));
    assert!(validator.supports(OPENAI_PROVIDER_NAME));
    assert!(validator.supports(GEMINI_PROVIDER_NAME));
    assert!(
        !validator.supports(OPENAI_COMPATIBLE_PROVIDER_NAME),
        "W5c must first carry base_url into compatible login validation"
    );

    let vault = Arc::new(MemoryVault::default());
    let anthropic_alias = CredentialAlias::new("anthropic-dispatch");
    let openai_alias = CredentialAlias::new("openai-dispatch");
    let gemini_alias = CredentialAlias::new("gemini-dispatch");
    let compatible_alias = CredentialAlias::new("compatible-dispatch");
    vault
        .put(&anthropic_alias, b"anthropic-fixture-secret")
        .unwrap_or_else(|error| panic!("{error:?}"));
    vault
        .put(&openai_alias, b"openai-fixture-secret")
        .unwrap_or_else(|error| panic!("{error:?}"));
    vault
        .put(&gemini_alias, b"gemini-fixture-secret")
        .unwrap_or_else(|error| panic!("{error:?}"));
    vault
        .put(&compatible_alias, b"compatible-fixture-secret")
        .unwrap_or_else(|error| panic!("{error:?}"));
    let snapshot = Arc::new(StdMutex::new(vec![
        CredentialDescriptor {
            alias: anthropic_alias.clone(),
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "anthropic fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
        CredentialDescriptor {
            alias: openai_alias.clone(),
            provider: OPENAI_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "openai fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
        CredentialDescriptor {
            alias: gemini_alias.clone(),
            provider: GEMINI_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "gemini fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
        CredentialDescriptor {
            alias: compatible_alias.clone(),
            provider: OPENAI_COMPATIBLE_PROVIDER_NAME.into(),
            base_url: Some("http://127.0.0.1:11434".into()),
            auth_method: AuthMethod::ApiKey,
            identity: "compatible fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
    ]));
    let factory = AccountsProviderFactory::new(
        snapshot,
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
    );
    let metadata = |provider: &str, model: &str| haider_protocol::session::SessionMetadataV1 {
        cwd: "/tmp/haider-provider-dispatch".into(),
        provider: provider.into(),
        model: model.into(),
        max_tokens: 64,
        permission_overrides: None,
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        created_at_ms: 1,
    };

    let openai = factory
        .resolve_for_turn(&metadata(OPENAI_PROVIDER_NAME, "gpt-5-test"))
        .await
        .unwrap_or_else(|error| panic!("openai dispatch: {error:?}"));
    let anthropic = factory
        .resolve_for_turn(&metadata(ANTHROPIC_PROVIDER_NAME, "claude-test"))
        .await
        .unwrap_or_else(|error| panic!("anthropic dispatch: {error:?}"));
    let gemini = factory
        .resolve_for_turn(&metadata(GEMINI_PROVIDER_NAME, "gemini-2.5-flash"))
        .await
        .unwrap_or_else(|error| panic!("gemini dispatch: {error:?}"));
    let compatible = factory
        .resolve_for_turn(&metadata(OPENAI_COMPATIBLE_PROVIDER_NAME, "llama-test"))
        .await
        .unwrap_or_else(|error| panic!("compatible dispatch: {error:?}"));

    assert_eq!(
        openai.provider.capabilities().await.provider,
        OPENAI_PROVIDER_NAME
    );
    assert_eq!(openai.account_alias.as_deref(), Some(openai_alias.as_str()));
    assert_eq!(
        anthropic.provider.capabilities().await.provider,
        ANTHROPIC_PROVIDER_NAME
    );
    assert_eq!(
        anthropic.account_alias.as_deref(),
        Some(anthropic_alias.as_str())
    );
    assert_eq!(
        gemini.provider.capabilities().await.provider,
        GEMINI_PROVIDER_NAME
    );
    assert_eq!(gemini.account_alias.as_deref(), Some(gemini_alias.as_str()));
    assert_eq!(
        compatible.provider_name, OPENAI_COMPATIBLE_PROVIDER_NAME,
        "successful resolution pins the compatible dispatch arm"
    );
    assert_eq!(
        compatible.account_alias.as_deref(),
        Some(compatible_alias.as_str())
    );
}

/// WH1 factory half — the named DeepSeek API-key pair constructs through
/// the dedicated compatible adapter surface; OAuth cannot cross-wire it.
#[test]
fn wh1_deepseek_factory_builds_api_key_adapter() {
    let validator = ProviderCredentialValidator;
    assert!(validator.supports(DEEPSEEK_PROVIDER_NAME));
    let vault = MemoryVault::default();
    let alias = CredentialAlias::new("deepseek-factory");
    vault
        .put(&alias, b"DEEPSEEK_FACTORY_KEY_SENTINEL_7ad1")
        .expect("store DeepSeek factory key");
    let adapter = build_account_provider(
        DEEPSEEK_PROVIDER_NAME,
        None,
        None,
        AuthMethod::ApiKey,
        vault.resolve(&alias).expect("resolve DeepSeek key"),
        "deepseek-reasoner",
        &alias,
        &ProviderTuning::default(),
        None,
    )
    .expect("build DeepSeek adapter");
    assert_eq!(
        adapter.credential_surface(),
        haider_provider::ProviderCredentialSurface::ApiKey
    );

    let oauth = build_account_provider(
        DEEPSEEK_PROVIDER_NAME,
        None,
        None,
        AuthMethod::OAuth,
        vault.resolve(&alias).expect("resolve DeepSeek key again"),
        "deepseek-reasoner",
        &alias,
        &ProviderTuning::default(),
        None,
    );
    let Err(oauth) = oauth else {
        panic!("DeepSeek OAuth must not cross-wire");
    };
    assert_eq!(oauth.code, ErrorCode::InvalidArgument);
}

/// The production validation path deliberately remains a real provider
/// smoke: it creates the native Gemini adapter and sends the one-token ping
/// built by `validate_provider_api_key`. It is ignored unless explicitly
/// selected with a live key.
///
/// MUTATION CHECK: route Gemini through another adapter, raise max_tokens
/// above one, or surface a provider body in validation errors. Expected
/// RUNTIME failure: the live request shape/behavior changes or the key
/// sentinel appears in the public error assertion.
#[tokio::test]
#[ignore = "live Gemini validator ping; requires HAIDER_LIVE_PROVIDER_TESTS=1"]
async fn validator_ping_uses_real_adapter_and_stores_no_secret_in_errors() {
    if std::env::var("HAIDER_LIVE_PROVIDER_TESTS").as_deref() != Ok("1") {
        return;
    }
    let secret = std::env::var("HAIDER_GEMINI_API_KEY").expect("live Gemini API key");
    let model =
        std::env::var("HAIDER_GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_owned());
    let result = ProviderCredentialValidator
        .validate(GEMINI_PROVIDER_NAME, &model, secret.as_bytes(), None)
        .await;
    if let Err(error) = &result {
        assert!(!error.message.contains(&secret));
    }
    result.expect("live Gemini one-token validation ping");
}

/// MUTATION CHECK (W5g-5b): remove the profile API-family dispatch arm from
/// `build_account_provider`. Expected runtime failure: resolving
/// `custom-llama` returns "no account-backed adapter" instead of constructing
/// the compatible adapter from the stored profile origin.
///
/// MUTATION CHECK: prefer the credential descriptor's `base_url` over the
/// profile endpoint. Expected runtime failure: construction rejects the
/// descriptor's remote HTTP decoy rather than succeeding with the stored
/// loopback profile origin. The fixed-name assertion also pins the legacy
/// credential-owned fallback.
#[tokio::test]
async fn custom_chat_completions_profile_routes_with_profile_origin_and_legacy_fallback() {
    let provider = "custom-llama";
    let alias = CredentialAlias::new("custom-llama-key");
    let descriptor = CredentialDescriptor {
        alias: alias.clone(),
        provider: provider.to_owned(),
        base_url: Some("http://203.0.113.7/v1".to_owned()),
        auth_method: AuthMethod::ApiKey,
        identity: "custom fixture".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let summary = ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:11434/v1".to_owned()),
        models: vec!["llama-fixture".to_owned()],
        model_details: vec![
            ModelDetailWire {
                name: "llama-fixture".to_owned(),
                context_window: Some(131_072),
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
            },
            ModelDetailWire {
                name: "llama-other".to_owned(),
                context_window: Some(65_536),
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
            },
        ],
        auth_methods: vec![AuthMethod::ApiKey],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("llama-fixture".to_owned()),
        enabled: true,
    };
    assert_eq!(
        account_openai_compatible_base_url(
            provider,
            Some(&summary),
            descriptor.base_url.as_deref()
        ),
        Some("http://127.0.0.1:11434/v1"),
        "the registry profile origin outranks credential metadata"
    );
    assert_eq!(
        account_openai_compatible_base_url(
            OPENAI_COMPATIBLE_PROVIDER_NAME,
            Some(&summary),
            descriptor.base_url.as_deref()
        ),
        descriptor.base_url.as_deref(),
        "the fixed-name legacy adapter remains credential-addressed"
    );

    let vault = Arc::new(MemoryVault::default());
    vault
        .put(&alias, b"custom-compatible-fixture-secret")
        .expect("seed custom key");
    let snapshot = Arc::new(StdMutex::new(vec![descriptor]));
    let management =
        ManagementSnapshot::new(0, snapshot.lock().expect("snapshot").clone(), vec![summary]);
    let factory = AccountsProviderFactory::new_with_management(
        snapshot,
        management,
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
    );
    let resolved = factory
        .resolve_for_turn(&haider_protocol::session::SessionMetadataV1 {
            cwd: "/tmp/custom-family-dispatch".to_owned(),
            provider: provider.to_owned(),
            model: "llama-fixture".to_owned(),
            max_tokens: 64,
            permission_overrides: None,
            system_prompt_version: None,
            title: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            created_at_ms: 1,
        })
        .await
        .expect("custom family dispatch");
    assert_eq!(resolved.provider_name, provider);
    assert_eq!(resolved.account_alias.as_deref(), Some(alias.as_str()));
    assert_eq!(resolved.context_window, Some(131_072));
    assert_eq!(
        factory.model_context_window(provider, "not-in-the-catalog"),
        None,
        "context windows are exact provider/model catalog facts, never fallbacks"
    );
}

fn keyless_summary(provider: &str, origin: &str) -> ProviderSummaryWire {
    ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some(origin.to_owned()),
        models: vec!["llama3.1:8b".to_owned()],
        model_details: vec![ModelDetailWire {
            name: "llama3.1:8b".to_owned(),
            context_window: None,
            supported_efforts: Vec::new(),
            default_effort: None,
            supported_speeds: Vec::new(),
            supports_thinking_type: None,
        }],
        auth_methods: Vec::new(),
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("llama3.1:8b".to_owned()),
        enabled: true,
    }
}

/// LAW (LK1 — keyless auth arm, G4a): an ENABLED custom chat-completions
/// profile whose auth requirement is None resolves a turn provider WITHOUT
/// any stored credential — the synthesized `{provider}-keyless` account with
/// the placeholder bearer `ollama` (the compat-layer convention; LM Studio
/// ignores it) built by the factory's keyless arm at the profile origin.
/// When the user DOES store a key for the same profile, the stored key wins:
/// resolution returns the vault-backed alias, never the synthesized one.
///
/// MUTATION CHECK: delete the `keyless_account` fallback in
/// `resolve_provider`. Expected runtime failure: the no-credential
/// resolution below reports CredentialMissing.
/// MUTATION CHECK: delete the keyless (empty-auth) arm from
/// `build_account_provider`. Expected runtime failure: both resolutions
/// below report "no account-backed adapter" (the key-requiring arm
/// deliberately excludes empty-auth profiles).
/// MUTATION CHECK: change `KEYLESS_PLACEHOLDER_BEARER`. Expected runtime
/// failure: the placeholder equality below.
#[tokio::test]
async fn lk1_keyless_profile_resolves_placeholder_and_stored_key_wins() {
    let provider = "ollama";
    let origin = "http://127.0.0.1:11434/v1";
    let summary = keyless_summary(provider, origin);
    let metadata = haider_protocol::session::SessionMetadataV1 {
        cwd: "/tmp/keyless-dispatch".to_owned(),
        provider: provider.to_owned(),
        model: "llama3.1:8b".to_owned(),
        max_tokens: 64,
        permission_overrides: None,
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        created_at_ms: 1,
    };

    // The placeholder rides the same handle machinery as real secrets and
    // carries exactly the compat convention bytes.
    assert_eq!(KEYLESS_PLACEHOLDER_BEARER, b"ollama");
    let placeholder = keyless_placeholder_credential().expect("placeholder credential");
    assert_eq!(placeholder.expose_secret(), b"ollama");

    // NO stored credential anywhere: resolution synthesizes the keyless
    // account instead of failing CredentialMissing.
    let vault = Arc::new(MemoryVault::default());
    let snapshot = Arc::new(StdMutex::new(Vec::new()));
    let management = ManagementSnapshot::new(0, Vec::new(), vec![summary.clone()]);
    let factory = AccountsProviderFactory::new_with_management(
        snapshot,
        management,
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
    );
    let resolved = factory
        .resolve_for_turn(&metadata)
        .await
        .expect("keyless dispatch");
    assert_eq!(resolved.provider_name, provider);
    assert_eq!(resolved.account_alias.as_deref(), Some("ollama-keyless"));
    assert!(resolved.initial_rotation.is_none());

    // A STORED KEY WINS: with an active vault-backed descriptor for the same
    // auth-None profile, resolution returns it and never synthesizes.
    let alias = CredentialAlias::new("ollama-key");
    let vault = Arc::new(MemoryVault::default());
    vault
        .put(&alias, b"stored-key-sentinel")
        .expect("seed stored key");
    let descriptor = CredentialDescriptor {
        alias: alias.clone(),
        provider: provider.to_owned(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "stored ollama key".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let snapshot = Arc::new(StdMutex::new(vec![descriptor.clone()]));
    let management = ManagementSnapshot::new(0, vec![descriptor], vec![summary]);
    let factory = AccountsProviderFactory::new_with_management(
        snapshot,
        management,
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
    );
    let resolved = factory
        .resolve_for_turn(&metadata)
        .await
        .expect("stored-key dispatch");
    assert_eq!(resolved.account_alias.as_deref(), Some(alias.as_str()));
}

/// LAW (LK1 refusal edge): the keyless fallback is SCOPED — a provider whose
/// profile requires a key (auth_methods non-empty) still fails
/// CredentialMissing with no stored credential, and a DISABLED auth-None
/// profile is not served either.
#[tokio::test]
async fn lk1_keyless_fallback_stays_scoped_to_enabled_auth_none_profiles() {
    let keyed = ProviderSummaryWire {
        auth_methods: vec![AuthMethod::ApiKey],
        ..keyless_summary("hf-proxy", "http://127.0.0.1:9000/v1")
    };
    let disabled = ProviderSummaryWire {
        enabled: false,
        ..keyless_summary("ollama-off", "http://127.0.0.1:11434/v1")
    };
    for summary in [keyed, disabled] {
        let provider = summary.provider.clone();
        let factory = AccountsProviderFactory::new_with_management(
            Arc::new(StdMutex::new(Vec::new())),
            ManagementSnapshot::new(0, Vec::new(), vec![summary]),
            VaultProvision::Available(Arc::new(MemoryVault::default()) as Arc<dyn Vault>),
            Arc::new(ProductionAccountBuilder),
        );
        let Err(error) = factory
            .resolve_for_turn(&haider_protocol::session::SessionMetadataV1 {
                cwd: "/tmp/keyless-scope".to_owned(),
                provider: provider.clone(),
                model: "llama3.1:8b".to_owned(),
                max_tokens: 64,
                permission_overrides: None,
                system_prompt_version: None,
                title: None,
                effort: None,
                fast: false,
                cache_policy: Default::default(),
                created_at_ms: 1,
            })
            .await
        else {
            panic!("out-of-scope profile `{provider}` must not resolve keyless");
        };
        assert_eq!(error.code, ErrorCode::CredentialMissing, "{provider}");
    }
}

/// LAW (LK2 — preset configure + discovery, G4a): a keyless preset's
/// `provider.configure` persists the registry profile with the exact
/// identity (chat-completions family, auth None, Custom provenance, the
/// stated origin); `catalog_source` routes it to the credential-free
/// OpenAI-compatible source; and PRODUCTION discovery against a mock
/// loopback `/v1/models` populates the inventory and flips the summary
/// Available. Before discovery the enabled profile is honestly Unavailable
/// (the inventory rule).
///
/// MUTATION CHECK: refuse auth-None customs in `catalog_source` (require
/// ApiKey). Expected runtime failure: the source assertion below.
/// MUTATION CHECK: require a credential for compatible discovery. Expected
/// runtime failure: `discover_models(source, None, None)` errors.
#[tokio::test]
async fn lk2_keyless_preset_configure_persists_and_mock_discovery_flips_available() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind mock models server");
    let origin = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().expect("mock addr").port()
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept discovery");
        let mut request = [0_u8; 2048];
        let _ = socket.read(&mut request).await.expect("read discovery");
        let body = br#"{"object":"list","data":[{"id":"llama3.1:8b"},{"id":"qwen3:4b"}]}"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.expect("write head");
        socket.write_all(body).await.expect("write body");
    });

    let store: Box<dyn ProviderRegistryStoreLike> = Box::new(TestProviderStore::default());
    let mut providers = ProviderRegistry::new(
        store,
        Vec::new(),
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("registry");
    let profile = providers
        .configure(ProviderConfigureInput {
            provider: "ollama".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some(origin.clone()),
            auth_requirement: Some(ProviderAuthRequirementWire::None),
            enabled: true,
            models: vec!["llama3.1:8b".to_owned()],
            default_model: Some("llama3.1:8b".to_owned()),
        })
        .expect("keyless preset configure");
    assert_eq!(
        profile.api_family,
        ProviderApiFamilyWire::OpenAiChatCompletions
    );
    assert_eq!(profile.auth_requirement, ProviderAuthRequirementWire::None);
    assert_eq!(profile.provenance, ProviderProvenance::Custom);
    assert_eq!(profile.base_url.as_deref(), Some(origin.as_str()));

    let (source, auth) = catalog_source("ollama", &providers).expect("keyless catalog source");
    assert_eq!(
        source,
        CatalogSource::OpenAiCompatible {
            origin: origin.clone()
        }
    );
    assert_eq!(auth, ProviderAuthRequirementWire::None);

    let before = providers
        .summary("ollama", &|_| false)
        .expect("pre-discovery summary");
    assert_eq!(
        before.availability,
        haider_rpc::ProviderAvailabilityWire::Unavailable,
        "no discovered inventory yet"
    );
    assert!(before.auth_methods.is_empty());

    let catalog = discover_models(source, None, None)
        .await
        .expect("credential-free mock discovery");
    server.await.expect("mock server served one request");
    providers.replace_models("ollama".to_owned(), catalog.models);

    let after = providers
        .summary("ollama", &|_| false)
        .expect("post-discovery summary");
    assert_eq!(
        after.availability,
        haider_rpc::ProviderAvailabilityWire::Available
    );
    assert_eq!(
        after.models,
        vec!["llama3.1:8b".to_owned(), "qwen3:4b".to_owned()]
    );
    assert_eq!(after.default_model.as_deref(), Some("llama3.1:8b"));
}

/// MUTATION CHECK (review of record, W5b retrospective): weaken any component
/// of the generation/issuer/audience/resource/subject-hash fence in
/// `apply_oauth_refresh`, `expire_oauth_refresh`, or `begin_oauth_refresh`.
/// Expected failure: the late apply overwrites the replacement bundle in the
/// vault, or a stale-fence expiry/begin reports `true` and marks the
/// replacement `Expired`.
///
/// This pins the PRODUCTION account actor. The `late_refresh_*` tests in
/// `oauth_tests.rs` drive `start_status_actor`, a test double that
/// reimplements this fence, so they do not cover these functions at all —
/// the entire CAS could be deleted at all three sites with the daemon suite
/// still green.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn stale_oauth_fences_cannot_overwrite_or_expire_a_replaced_bundle() {
    let bundle_at = |generation: u64, access: &[u8]| {
        haider_accounts::OAuthTokenBundleV1::new(
            "fake-oauth".into(),
            "http://127.0.0.1:32109".into(),
            "fake-audience".into(),
            Some("fake-api-resource".into()),
            "Bearer".into(),
            Zeroizing::new(access.to_vec()),
            Some(Zeroizing::new(
                b"OAUTH_REFRESH_FENCE_SENTINEL_4b7e".to_vec(),
            )),
            u64::MAX - 1,
            Some(u64::MAX),
            vec!["openid".into(), "inference".into()],
            haider_accounts::OAuthIdentityV1 {
                subject_hash: "verified-subject".into(),
                display_identity: "person@example.invalid".into(),
            },
            generation,
        )
        .expect("OAuth bundle")
    };
    let fence_for = |bundle: &haider_accounts::OAuthTokenBundleV1| OAuthRefreshFence {
        fence_epoch: 0,
        generation: bundle.generation,
        issuer: bundle.issuer.clone(),
        audience: bundle.audience.clone(),
        resource: bundle.resource.clone(),
        subject_hash: bundle.identity.subject_hash.clone(),
    };

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let alias = CredentialAlias::new("fenced-oauth");
    let descriptor = CredentialDescriptor {
        alias: alias.clone(),
        provider: "fake-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "person@example.invalid".into(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let mut accounts = memory_accounts();
    accounts.add(descriptor.clone()).expect("descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());

    // The vault already holds generation 2: a concurrent refresh or re-login
    // replaced the bundle while an older refresh was still in flight.
    let replacement = bundle_at(2, b"ACCESS_REPLACEMENT_SENTINEL_c0de");
    vault
        .put(&alias, &replacement.encode().expect("encode replacement"))
        .expect("seed replacement");
    let superseded = bundle_at(1, b"ACCESS_SUPERSEDED_SENTINEL_0old");
    let late = bundle_at(1, b"ACCESS_LATE_SENTINEL_dead");

    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "oauth-fence".into(),
        default_model: "gpt-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();

    let (completed, applied) = tokio::sync::oneshot::channel();
    commands
        .send(AccountCommand::ApplyOAuthRefresh {
            descriptor: descriptor.clone(),
            expected: fence_for(&superseded),
            encoded_bundle: late.encode().expect("encode late"),
            completed,
        })
        .await
        .expect("apply command");
    assert!(matches!(
        applied.await.expect("apply response"),
        Err(RefreshApplyError::Stale)
    ));

    let stored = vault.resolve(&alias).expect("stored bundle");
    let stored =
        haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret()).expect("decode stored");
    assert_eq!(
        stored.generation, 2,
        "replacement must survive a late apply"
    );
    assert_eq!(
        stored.access_token_handle().expose_secret(),
        b"ACCESS_REPLACEMENT_SENTINEL_c0de"
    );

    let (completed, expired) = tokio::sync::oneshot::channel();
    commands
        .send(AccountCommand::ExpireOAuthRefresh {
            descriptor: descriptor.clone(),
            expected: fence_for(&superseded),
            completed,
        })
        .await
        .expect("expire command");
    assert!(
        !expired.await.expect("expire response").expect("no error"),
        "a stale fence must not expire the replacement"
    );

    let (completed, began) = tokio::sync::oneshot::channel();
    commands
        .send(AccountCommand::BeginOAuthRefresh {
            descriptor: descriptor.clone(),
            expected: fence_for(&superseded),
            completed,
        })
        .await
        .expect("begin command");
    assert!(
        !began.await.expect("begin response").expect("no error"),
        "a stale fence must not open a refresh against the replacement"
    );

    {
        let current = snapshot.lock().expect("snapshot");
        assert!(
            current
                .iter()
                .any(|entry| entry.alias == alias && entry.status == CredentialStatus::Ok),
            "the replacement descriptor must remain usable"
        );
    }

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK (review of record, W5c.2b): disable both reserved-alias
/// fences (`if reserved_aliases.contains(alias.as_str())` in `handle_login`
/// and `handle_oauth_add`, e.g. via `if false && …`). Expected runtime
/// failure: both commands below succeed instead of answering the retryable
/// `busy` rejection, so a login could re-occupy an alias whose removal
/// cleanup is still pending — a crash between vault-delete retries would then
/// delete the NEW credential's secret.
/// Verified by revert on 2026-07-30.
#[tokio::test]
async fn reserved_alias_fences_login_and_oauth_add_until_remove_finalizes() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let vault = Arc::new(MemoryVault::new());
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "reserved-fence".into(),
        default_model: "claude-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::from(["work".to_owned(), "work-oauth".to_owned()]),
        refresh_fences: RefreshFenceRegistry::default(),
    });

    let (sink, mut frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::Login(Box::new(LoginJob {
            command_id: "reserved-login".into(),
            provider: "anthropic".into(),
            display_alias: Some("work".into()),
            validation_model: Some("claude-test".into()),
            secret: Some(Zeroizing::new(b"sk-reserved".to_vec())),
            route: LoginRoute {
                request_id: RequestId::new("reserved-login-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("send login");
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("login response deadline")
        .expect("login response");
    match frame {
        WireFrame::Response {
            body: ResponseBody::Error {
                code, retryable, ..
            },
            ..
        } => {
            assert_eq!(code, ERROR_CODE_BUSY, "reserved alias must answer busy");
            assert!(
                retryable,
                "removal cleanup is transient — must be retryable"
            );
        }
        other => panic!("reserved login must be rejected, got {other:?}"),
    }
    assert!(
        snapshot.lock().expect("snapshot").is_empty(),
        "no descriptor may be committed for a reserved alias"
    );

    actor
        .commands()
        .send(AccountCommand::AddOAuth(Box::new(OAuthAddJob {
            command_id: "reserved-oauth".into(),
            provider: "fake-oauth".into(),
            display_alias: "work-oauth".into(),
            claim: Some(OAuthReadyClaim::for_account_test(
                "fake-oauth",
                "work-oauth",
                oauth_bundle(),
            )),
            route: LoginRoute {
                request_id: RequestId::new("reserved-oauth-request"),
                sink,
            },
        })))
        .await
        .expect("send oauth add");
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("oauth response deadline")
        .expect("oauth response");
    match frame {
        WireFrame::Response {
            body: ResponseBody::Error {
                code, retryable, ..
            },
            ..
        } => {
            assert_eq!(code, ERROR_CODE_BUSY);
            assert!(retryable);
        }
        other => panic!("reserved oauth add must be rejected, got {other:?}"),
    }
    assert!(
        vault.list().expect("vault list").is_empty(),
        "no secret may be written for a reserved alias"
    );

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK (review of record, W5c.1): drop the `error.retryable` arm
/// from the attempt resolver's rotation-failure match so the resolver error
/// propagates. Expected failure: the retryable bookkeeping error escapes as
/// `Err`, the decision is never `Wait`, and the turn dies on an error the
/// provider itself said to retry.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn retryable_rotation_bookkeeping_failure_waits_instead_of_killing_the_turn() {
    struct WriteRefusingStore(Vec<CredentialDescriptor>);
    impl StoreLike for WriteRefusingStore {
        fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
            Ok(self.0.clone())
        }
        fn save(&self, _descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
            Err(HaiderError::new(
                ErrorCode::StoreLocked,
                "descriptor store is temporarily locked",
                true,
            ))
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let primary_alias = CredentialAlias::new("wedged-primary");
    let alternate_alias = CredentialAlias::new("wedged-backup");
    let descriptor = |alias: CredentialAlias, active: bool| CredentialDescriptor {
        alias,
        provider: OPENAI_PROVIDER_NAME.into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "wedged fixture".into(),
        status: CredentialStatus::Ok,
        active,
    };
    let descriptors = vec![
        descriptor(primary_alias.clone(), true),
        descriptor(alternate_alias.clone(), false),
    ];
    let backing: Box<dyn StoreLike> = Box::new(WriteRefusingStore(descriptors.clone()));
    let accounts = AccountStore::new(backing).expect("accounts");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(descriptors));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(&alternate_alias, b"wedged-alternate-secret")
        .expect("alternate secret");
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "wedged-store".into(),
        default_model: "gpt-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::default(),
        Arc::clone(&snapshot),
        actor.commands(),
    )
    .expect("broker");
    let factory = AccountsProviderFactory::with_broker(
        Arc::clone(&snapshot),
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
        broker.clone(),
    );
    let resolver = AccountsAttemptResolver::new(
        factory,
        haider_protocol::session::SessionMetadataV1 {
            cwd: "/tmp/wedged-store".into(),
            provider: OPENAI_PROVIDER_NAME.into(),
            model: "gpt-test".into(),
            max_tokens: 64,
            permission_overrides: None,
            system_prompt_version: None,
            title: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            created_at_ms: 1,
        },
        ProviderTuning::default(),
        None,
    );

    let decision = resolver
        .resolve(
            &primary_alias,
            &haider_provider::ProviderError::new(
                haider_provider::ProviderErrorKind::RateLimited,
                "bounded rate limit",
            )
            .with_retry_after_ms(Some(1_000)),
        )
        .await
        .expect("a retryable bookkeeping failure is not a turn-fatal error");
    assert!(matches!(
        decision,
        haider_core::ProviderAttemptDecision::Wait
    ));

    assert!(broker.shutdown().await);
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK (W5c.1 resolver-backed factory): bypass
/// `CredentialBroker::resolve_account` and restore direct active-snapshot
/// resolution. Expected failure: the limited active alias is rejected or
/// returned instead of the checked one-hop alternate, and no durable
/// selection/rotation metadata is visible.
/// Verified by revert (direct bypass and fabricated auth deadline) on
/// 2026-07-29.
#[tokio::test]
async fn factory_uses_checked_resolver_and_durably_selects_one_limited_alternate() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let mut accounts = memory_accounts();
    let limited_alias = CredentialAlias::new("limited-primary");
    let alternate_alias = CredentialAlias::new("usable-backup");
    let refresh_failed_alias = CredentialAlias::new("refresh-failed-primary");
    let refresh_backup_alias = CredentialAlias::new("refresh-usable-backup");
    accounts
        .add(CredentialDescriptor {
            alias: limited_alias.clone(),
            provider: OPENAI_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "limited fixture".into(),
            status: CredentialStatus::Limited { until_ms: u64::MAX },
            active: true,
        })
        .expect("limited descriptor");
    accounts
        .add(CredentialDescriptor {
            alias: alternate_alias.clone(),
            provider: OPENAI_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "backup fixture".into(),
            status: CredentialStatus::Ok,
            active: false,
        })
        .expect("alternate descriptor");
    accounts
        .add(CredentialDescriptor {
            alias: refresh_failed_alias.clone(),
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: "refresh failed fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        })
        .expect("refresh-failed descriptor");
    accounts
        .add(CredentialDescriptor {
            alias: refresh_backup_alias.clone(),
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "refresh backup fixture".into(),
            status: CredentialStatus::Ok,
            active: false,
        })
        .expect("refresh backup descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(&alternate_alias, b"checked-alternate-secret")
        .expect("alternate vault secret");
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "resolver-factory".into(),
        default_model: "gpt-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::default(),
        Arc::clone(&snapshot),
        actor.commands(),
    )
    .expect("broker");
    let factory = AccountsProviderFactory::with_broker(
        Arc::clone(&snapshot),
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
        broker.clone(),
    );
    let resolved = factory
        .resolve_for_turn(&haider_protocol::session::SessionMetadataV1 {
            cwd: "/tmp/resolver-factory".into(),
            provider: OPENAI_PROVIDER_NAME.into(),
            model: "gpt-test".into(),
            max_tokens: 64,
            permission_overrides: None,
            system_prompt_version: None,
            title: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            created_at_ms: 1,
        })
        .await
        .expect("checked alternate resolution");

    assert_eq!(
        resolved.account_alias.as_deref(),
        Some(alternate_alias.as_str())
    );
    assert!(resolved.rotation_budget_consumed);
    assert_eq!(
        resolved.initial_rotation,
        Some(haider_protocol::credential::RotationEvent {
            provider: OPENAI_PROVIDER_NAME.into(),
            from: limited_alias.clone(),
            to: alternate_alias.clone(),
            cause: haider_protocol::credential::RotationCause::RateLimit,
        })
    );
    let refresh_rotation = broker
        .resolve_account(
            ANTHROPIC_PROVIDER_NAME,
            Some((refresh_failed_alias.clone(), RotationTrigger::RefreshFailed)),
        )
        .await
        .expect("refresh failure uses checked alternate");
    assert_eq!(
        refresh_rotation.rotation,
        Some(haider_protocol::credential::RotationEvent {
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            from: refresh_failed_alias.clone(),
            to: refresh_backup_alias.clone(),
            cause: haider_protocol::credential::RotationCause::Error,
        })
    );
    {
        let current = snapshot.lock().expect("snapshot");
        assert!(
            current
                .iter()
                .any(|descriptor| descriptor.alias == alternate_alias && descriptor.active)
        );
        assert!(
            current
                .iter()
                .any(|descriptor| descriptor.alias == limited_alias && !descriptor.active)
        );
        assert!(current.iter().any(|descriptor| {
            descriptor.alias == refresh_failed_alias
                && matches!(descriptor.status, CredentialStatus::Expired)
                && !descriptor.active
        }));
        assert!(current.iter().any(|descriptor| {
            descriptor.alias == refresh_backup_alias
                && descriptor.status == CredentialStatus::Ok
                && descriptor.active
        }));
    }
    drop(factory);
    assert!(broker.shutdown().await);
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: dispatch either OAuth descriptor through its API-key arm,
/// remove the sanctioned-provider match, pass the encoded bundle to an
/// adapter, or omit OpenAI from the access-fingerprint handoff used by C2's
/// re-read-on-401 adoption. The broker/factory resolution below fails before
/// capabilities or the literal OpenAI fingerprint assertion becomes `None`.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn auth_aware_factory_routes_sanctioned_oauth_descriptors_to_subscription_adapters() {
    let vault = Arc::new(MemoryVault::default());
    let openai_alias = CredentialAlias::new("openai-oauth-dispatch");
    let anthropic_alias = CredentialAlias::new("anthropic-oauth-dispatch");
    let descriptor = |alias: CredentialAlias, provider: &str| CredentialDescriptor {
        alias,
        provider: provider.into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: format!("{provider} fixture"),
        status: CredentialStatus::Ok,
        active: true,
    };
    let openai_descriptor = descriptor(openai_alias.clone(), OPENAI_OAUTH_PROVIDER_NAME);
    let anthropic_descriptor = descriptor(anthropic_alias.clone(), ANTHROPIC_OAUTH_PROVIDER_NAME);
    let bundle = |provider: &str, issuer: &str, audience: &str, scopes: &[&str], access: &[u8]| {
        haider_accounts::OAuthTokenBundleV1::new(
            provider.into(),
            issuer.into(),
            audience.into(),
            None,
            "Bearer".into(),
            Zeroizing::new(access.to_vec()),
            Some(Zeroizing::new(b"REFRESH_FACTORY_SENTINEL_f0b1".to_vec())),
            u64::MAX - 2,
            Some(u64::MAX - 1),
            scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            haider_accounts::OAuthIdentityV1 {
                subject_hash: format!("{provider}-subject"),
                display_identity: format!("{provider} subscription"),
            },
            1,
        )
        .expect("OAuth bundle")
    };
    vault
        .put(
            &openai_alias,
            &bundle(
                OPENAI_OAUTH_PROVIDER_NAME,
                "https://auth.openai.com",
                "app_EMoamEEZ73f0CkXaXp7hrann",
                &["openid", "profile", "email", "offline_access"],
                b"OPENAI_FACTORY_ACCESS_SENTINEL_18a4",
            )
            .encode()
            .expect("encode OpenAI bundle"),
        )
        .expect("store OpenAI bundle");
    vault
        .put(
            &anthropic_alias,
            &bundle(
                ANTHROPIC_OAUTH_PROVIDER_NAME,
                "https://claude.ai",
                "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                // The stored grant must cover EVERY configured scope
                // (W5g-7 Claude Code parity set) or resolve refuses it.
                &[
                    "org:create_api_key",
                    "user:profile",
                    "user:inference",
                    "user:sessions:claude_code",
                    "user:mcp_servers",
                    "user:file_upload",
                ],
                b"ANTHROPIC_FACTORY_ACCESS_SENTINEL_d716",
            )
            .encode()
            .expect("encode Anthropic bundle"),
        )
        .expect("store Anthropic bundle");
    let snapshot = Arc::new(StdMutex::new(vec![
        openai_descriptor.clone(),
        anthropic_descriptor.clone(),
    ]));
    let (status_commands, mut status_receiver) = mpsc::channel(4);
    let resolver_snapshot = Arc::clone(&snapshot);
    let status_task = tokio::spawn(async move {
        while let Some(command) = status_receiver.recv().await {
            if let AccountCommand::ResolveCredential {
                provider,
                failure: None,
                completed,
            } = command
            {
                let result = resolver_snapshot
                    .lock()
                    .ok()
                    .and_then(|descriptors| {
                        descriptors
                            .iter()
                            .find(|descriptor| descriptor.provider == provider && descriptor.active)
                            .cloned()
                    })
                    .map(|descriptor| ResolvedAccount {
                        descriptor,
                        rotation: None,
                    })
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::CredentialMissing,
                            "test resolver has no active descriptor",
                            false,
                        )
                    });
                let _ = completed.send(result);
            }
        }
    });
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::default(),
        Arc::clone(&snapshot),
        status_commands,
    )
    .expect("credential broker");
    let factory = AccountsProviderFactory::with_broker(
        snapshot,
        VaultProvision::Available(vault.clone() as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
        broker,
    );
    let metadata = |provider: &str, model: &str| haider_protocol::session::SessionMetadataV1 {
        cwd: "/tmp/haider-oauth-dispatch".into(),
        provider: provider.into(),
        model: model.into(),
        max_tokens: 64,
        permission_overrides: None,
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        created_at_ms: 1,
    };
    let (_, _, openai_access_fingerprint) = factory
        .resolve_provider(
            &metadata(OPENAI_OAUTH_PROVIDER_NAME, "gpt-oauth"),
            &ProviderTuning::default(),
        )
        .await
        .expect("OpenAI OAuth fingerprint handoff");
    assert_eq!(
        openai_access_fingerprint,
        Some(*blake3::hash(b"OPENAI_FACTORY_ACCESS_SENTINEL_18a4").as_bytes())
    );
    let (_, _, anthropic_access_fingerprint) = factory
        .resolve_provider(
            &metadata(ANTHROPIC_OAUTH_PROVIDER_NAME, "claude-oauth"),
            &ProviderTuning::default(),
        )
        .await
        .expect("Anthropic conservative OAuth handoff");
    assert_eq!(anthropic_access_fingerprint, None);
    let openai = factory
        .resolve_for_turn(&metadata(OPENAI_OAUTH_PROVIDER_NAME, "gpt-oauth"))
        .await
        .expect("OpenAI OAuth dispatch");
    let anthropic = factory
        .resolve_for_turn(&metadata(ANTHROPIC_OAUTH_PROVIDER_NAME, "claude-oauth"))
        .await
        .expect("Anthropic OAuth dispatch");
    assert_eq!(openai.provider_name, OPENAI_OAUTH_PROVIDER_NAME);
    assert_eq!(
        openai.provider.credential_surface(),
        haider_provider::ProviderCredentialSurface::OAuthSubscriptionBearer
    );
    assert_eq!(
        openai.provider.capabilities().await.provider,
        OPENAI_PROVIDER_NAME
    );
    assert_eq!(anthropic.provider_name, ANTHROPIC_OAUTH_PROVIDER_NAME);
    assert_eq!(
        anthropic.provider.credential_surface(),
        haider_provider::ProviderCredentialSurface::OAuthSubscriptionBearer
    );
    assert_eq!(
        anthropic.provider.capabilities().await.provider,
        ANTHROPIC_PROVIDER_NAME
    );

    let wrong_alias = CredentialAlias::new("oauth-api-key-crosswire");
    vault
        .put(&wrong_alias, b"NEVER_CROSSWIRE_API_KEY_91f0")
        .expect("store crosswire key");
    let result = build_account_provider(
        OPENAI_OAUTH_PROVIDER_NAME,
        None,
        None,
        AuthMethod::ApiKey,
        vault.resolve(&wrong_alias).expect("resolve crosswire key"),
        "gpt-oauth",
        &wrong_alias,
        &crate::accounts::ProviderTuning::default(),
        None,
    );
    let Err(error) = result else {
        panic!("OAuth provider ID must reject API-key mode");
    };
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    drop((openai, anthropic, factory));
    status_task.await.expect("status task");
}

// MUTATION CHECK (R10 Keychain namespacing): remove the profile input from
// `physical_alias` (hash provider+command only). Expected failure: the two
// profiles below derive the SAME physical alias and the distinct-secret
// assertion fails.
#[test]
fn identical_display_aliases_in_two_profiles_resolve_distinct_secrets() {
    let a = physical_alias("profile-a", "anthropic", "command-1");
    let b = physical_alias("profile-b", "anthropic", "command-1");
    assert_ne!(a, b, "physical aliases must be profile-namespaced");
    assert_eq!(
        physical_alias("profile-a", "anthropic", "command-1"),
        a,
        "physical alias derivation must be stable for retries"
    );

    // Same display alias "work", two profiles, one shared vault backend:
    // the physical namespacing keeps the secrets distinct.
    let vault = MemoryVault::default();
    let alias_a = CredentialAlias::new(a);
    let alias_b = CredentialAlias::new(b);
    vault
        .put(&alias_a, b"secret-for-profile-a")
        .unwrap_or_else(|error| panic!("{error:?}"));
    vault
        .put(&alias_b, b"secret-for-profile-b")
        .unwrap_or_else(|error| panic!("{error:?}"));
    let resolved_a = vault
        .resolve(&alias_a)
        .unwrap_or_else(|error| panic!("{error:?}"));
    let resolved_b = vault
        .resolve(&alias_b)
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert_eq!(resolved_a.expose_secret(), b"secret-for-profile-a");
    assert_eq!(resolved_b.expose_secret(), b"secret-for-profile-b");
}

#[test]
fn canonical_login_identity_excludes_vault_reference_and_secret_material() {
    let identity = identity_for("profile-a", "command-1");
    let json = identity
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert!(json.contains("anthropic") && json.contains("claude-test"));
    assert!(
        !json.contains("vaultref") && !json.contains("secret"),
        "command identity must exclude the ephemeral reference and secrets: {json}"
    );
    // Round-trips for the reconciliation reader.
    let decoded: LoginIdentity =
        serde_json::from_str(&json).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(decoded, identity);
}

#[test]
fn staged_secrets_dedupe_by_stage_id_and_claims_are_single_use() {
    let mut stages = StagedSecrets::default();
    let (reference, expires) = stages
        .stage("stage-1", StagePurpose::ApiKey, b"sk-first")
        .unwrap_or_else(|_| panic!("stage"));
    assert!(expires > 0);
    // Same id + same bytes: the SAME reference (same-connection retry).
    let (again, _) = stages
        .stage("stage-1", StagePurpose::ApiKey, b"sk-first")
        .unwrap_or_else(|_| panic!("re-stage"));
    assert_eq!(reference, again);
    // Same id + different bytes: invalid.
    assert!(matches!(
        stages.stage("stage-1", StagePurpose::ApiKey, b"sk-other"),
        Err(StageError::Mismatch)
    ));
    // Claim is single-use.
    let (purpose, secret) = stages
        .claim(&reference)
        .unwrap_or_else(|| panic!("first claim"));
    assert_eq!(purpose, StagePurpose::ApiKey);
    assert_eq!(secret.as_slice(), b"sk-first");
    assert!(
        stages.claim(&reference).is_none(),
        "references are single-use"
    );
    // Unknown references never claim.
    assert!(stages.claim("vaultref-doesnotexist").is_none());
}

// MUTATION CHECK (R7 stage TTL): remove `sweep_expired` from `claim`.
// Expected failure: the expired reference below still claims.
#[tokio::test(start_paused = true)]
async fn staged_secrets_expire_after_the_five_minute_ttl() {
    let mut stages = StagedSecrets::default();
    let (reference, _) = stages
        .stage("stage-1", StagePurpose::ApiKey, b"sk-expiring")
        .unwrap_or_else(|_| panic!("stage"));
    tokio::time::advance(SECRET_TTL + Duration::from_secs(1)).await;
    assert!(
        stages.claim(&reference).is_none(),
        "an expired stage must be wiped, forcing restage_required"
    );
}

async fn open_store(dir: &std::path::Path) -> SqliteStoreHandle {
    SqliteStoreHandle::open(dir)
        .await
        .unwrap_or_else(|error| panic!("open store: {}", error.message))
}

// ─────────────────── pending-command secret TTL (R7/R10) ────────────────────

/// Correlated responses land in an unbounded channel (the account actor
/// never awaits delivery, so the sink must never block).
struct ChannelSink(mpsc::UnboundedSender<WireFrame>);

impl FrameSink for ChannelSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.send(frame).map_err(|_| FrameSendError)
    }
}

fn channel_sink() -> (Arc<dyn FrameSink>, mpsc::UnboundedReceiver<WireFrame>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (Arc::new(ChannelSink(sender)), receiver)
}

const OAUTH_IMPORT_ENV_CHILD: &str = "HAIDER_TEST_OAUTH_IMPORT_ENV_CHILD";

fn run_oauth_import_env_child(test_name: &str, overrides: &[(&str, &std::path::Path)]) -> bool {
    if std::env::var_os(OAUTH_IMPORT_ENV_CHILD).is_some() {
        return false;
    }
    let mut command = std::process::Command::new(
        std::env::current_exe().expect("current daemon test executable"),
    );
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(OAUTH_IMPORT_ENV_CHILD, "1");
    for (key, path) in overrides {
        command.env(key, path);
    }
    let output = command.output().expect("spawn isolated import test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("running 1 test"),
        "isolated import test failed or did not run\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

const CODEX_IMPORT_FIXTURE_1: &[u8] = br#"{
  "OPENAI_API_KEY": null,
  "tokens": {
    "id_token": "fake-id-token-1",
    "access_token": "fake-access-token-1",
    "refresh_token": "fake-refresh-token-1",
    "account_id": "fake-account-1"
  },
  "last_refresh": "2026-07-30T00:00:00Z"
}"#;

const CODEX_IMPORT_FIXTURE_2: &[u8] = br#"{
  "tokens": {
    "id_token": "fake-id-token-2",
    "access_token": "fake-access-token-2",
    "refresh_token": "fake-refresh-token-2",
    "account_id": "fake-account-1"
  }
}"#;

const CLAUDE_IMPORT_FIXTURE: &[u8] = br#"{
  "claudeAiOauth": {
    "accessToken": "fake-claude-access-token-1",
    "refreshToken": "fake-claude-refresh-token-1",
    "expiresAt": 4102444800123,
    "scopes": ["user:inference"],
    "subscriptionType": "max"
  }
}"#;

const CLAUDE_READ_THROUGH_FIXTURE: &[u8] = br#"{
  "claudeAiOauth": {
    "accessToken": "fake-claude-live-access-token-2",
    "refreshToken": "fake-claude-live-refresh-token-2",
    "expiresAt": 4102444800999,
    "scopes": ["user:inference"],
    "subscriptionType": "max"
  }
}"#;

struct StubAccountClaudeNative {
    bytes: StdMutex<Option<Vec<u8>>>,
    reads: std::sync::atomic::AtomicUsize,
}

impl StubAccountClaudeNative {
    fn with_bytes(bytes: &[u8]) -> Self {
        Self {
            bytes: StdMutex::new(Some(bytes.to_vec())),
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn unavailable() -> Self {
        Self {
            bytes: StdMutex::new(None),
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn replace(&self, bytes: &[u8]) {
        if let Ok(mut current) = self.bytes.lock() {
            *current = Some(bytes.to_vec());
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ClaudeNativeCredentialStore for StubAccountClaudeNative {
    fn read(
        &self,
        _event: ClaudeNativeReadEvent,
    ) -> Result<crate::oauth::ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.bytes
            .lock()
            .ok()
            .and_then(|bytes| bytes.clone())
            .map(|bytes| crate::oauth::ClaudeCredentialInput {
                location: std::path::PathBuf::from("mock native store: Claude Code-credentials"),
                bytes: Zeroizing::new(bytes),
                native_owner: true,
            })
            .ok_or(ClaudeNativeCredentialFailure::Missing)
    }
}

struct CountingAnthropicRefreshExchange {
    calls: std::sync::atomic::AtomicUsize,
    refresh_token_fingerprints: StdMutex<Vec<[u8; 32]>>,
}

impl CountingAnthropicRefreshExchange {
    fn new() -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            refresh_token_fingerprints: StdMutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn refresh_token_fingerprints(&self) -> Vec<[u8; 32]> {
        self.refresh_token_fingerprints
            .lock()
            .map(|fingerprints| fingerprints.clone())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl OAuthRefreshExchange for CountingAnthropicRefreshExchange {
    async fn exchange(
        &self,
        _client: &reqwest::Client,
        _registration: &OAuthProviderRegistration,
        refresh_token: &[u8],
        _device_id: Option<&SecretHandle>,
    ) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut fingerprints) = self.refresh_token_fingerprints.lock() {
            fingerprints.push(*blake3::hash(refresh_token).as_bytes());
        }
        Ok(Zeroizing::new(
            br#"{"access_token":"fake-anthropic-grant-access-token","refresh_token":"fake-anthropic-grant-refresh-token","token_type":"Bearer","expires_in":3600,"scope":"user:inference"}"#
                .to_vec(),
        ))
    }
}

#[derive(Clone, Copy)]
enum ImportRefreshMode {
    Success,
    InvalidGrant,
}

struct ImportRefreshServer {
    address: std::net::SocketAddr,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    refresh_token_fingerprints: Arc<StdMutex<Vec<[u8; 32]>>>,
    gate: Option<Arc<Semaphore>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ImportRefreshServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ImportRefreshServer {
    async fn start(mode: ImportRefreshMode, gated: bool) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind import refresh server");
        let address = listener.local_addr().expect("import refresh address");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let refresh_token_fingerprints = Arc::new(StdMutex::new(Vec::new()));
        let gate = gated.then(|| Arc::new(Semaphore::new(0)));
        let calls_for_task = Arc::clone(&calls);
        let refresh_token_fingerprints_for_task = Arc::clone(&refresh_token_fingerprints);
        let gate_for_task = gate.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let calls = Arc::clone(&calls_for_task);
                let refresh_token_fingerprints = Arc::clone(&refresh_token_fingerprints_for_task);
                let gate = gate_for_task.clone();
                tokio::spawn(async move {
                    serve_import_refresh(stream, mode, calls, refresh_token_fingerprints, gate)
                        .await;
                });
            }
        });
        Self {
            address,
            calls,
            refresh_token_fingerprints,
            gate,
            task,
        }
    }

    fn catalog(&self) -> OAuthProviderCatalog {
        import_refresh_catalog(&format!("http://{}/token", self.address))
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn refresh_token_fingerprints(&self) -> Vec<[u8; 32]> {
        self.refresh_token_fingerprints
            .lock()
            .map(|fingerprints| fingerprints.clone())
            .unwrap_or_default()
    }

    async fn wait_for_calls(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.calls() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("import refresh call count");
    }

    fn release(&self) {
        if let Some(gate) = &self.gate {
            gate.add_permits(8);
        }
    }
}

fn import_refresh_catalog(token_endpoint: &str) -> OAuthProviderCatalog {
    let registration = OAuthProviderRegistration::new(
        OPENAI_OAUTH_PROVIDER_NAME,
        "https://auth.openai.com",
        "http://127.0.0.1:1/authorize",
        token_endpoint,
        "app_EMoamEEZ73f0CkXaXp7hrann",
        ["openid", "profile", "email", "offline_access"].map(str::to_owned),
        "app_EMoamEEZ73f0CkXaXp7hrann",
        None,
        true,
        Arc::new(UnusedIdentityVerifier),
    )
    .expect("import refresh registration")
    .with_test_refresh_shape(OAuthTokenRequestEncoding::Json, false);
    OAuthProviderCatalog::with_test_registrations([registration]).expect("import refresh catalog")
}

fn anthropic_import_refresh_catalog(token_endpoint: &str) -> OAuthProviderCatalog {
    let registration = OAuthProviderRegistration::new(
        ANTHROPIC_OAUTH_PROVIDER_NAME,
        "https://claude.ai",
        "http://127.0.0.1:1/authorize",
        token_endpoint,
        crate::oauth::CLAUDE_DEFAULT_CLIENT_ID,
        ["user:inference".to_owned()],
        crate::oauth::CLAUDE_DEFAULT_CLIENT_ID,
        None,
        true,
        Arc::new(UnusedIdentityVerifier),
    )
    .expect("Anthropic import refresh registration")
    .with_test_refresh_shape(OAuthTokenRequestEncoding::Json, false);
    OAuthProviderCatalog::with_test_registrations([registration])
        .expect("Anthropic import refresh catalog")
}

async fn serve_import_refresh(
    mut stream: TcpStream,
    mode: ImportRefreshMode,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    refresh_token_fingerprints: Arc<StdMutex<Vec<[u8; 32]>>>,
    gate: Option<Arc<Semaphore>>,
) {
    let mut request = Vec::new();
    let body_start = loop {
        let mut chunk = [0_u8; 1024];
        let Ok(read) = stream.read(&mut chunk).await else {
            return;
        };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= header_end.saturating_add(4).saturating_add(content_length) {
            break header_end.saturating_add(4);
        }
    };
    if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request[body_start..])
        && let Some(refresh_token) = body
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
        && let Ok(mut fingerprints) = refresh_token_fingerprints.lock()
    {
        fingerprints.push(*blake3::hash(refresh_token.as_bytes()).as_bytes());
    }
    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if let Some(gate) = gate {
        gate.acquire()
            .await
            .expect("release gated import refresh")
            .forget();
    }
    let (status, body) = match mode {
        ImportRefreshMode::Success => (
            "200 OK",
            r#"{"access_token":"fake-refreshed-access-token","refresh_token":"fake-rotated-refresh-token","token_type":"Bearer","expires_in":3600,"scope":"openid profile email offline_access"}"#,
        ),
        ImportRefreshMode::InvalidGrant => (
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"expired fixture grant"}"#,
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn start_oauth_import_test_actor(
    store: &SqliteStoreHandle,
    vault: Arc<MemoryVault>,
    reserved_aliases: HashSet<String>,
    refresh_fences: RefreshFenceRegistry,
) -> (AccountActorHandle, AccountsSnapshot, ManagementSnapshot) {
    let accounts = memory_accounts();
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let providers = test_provider_registry();
    let management = ManagementSnapshot::new(0, Vec::new(), providers.summaries(&|_| false));
    let actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: vault as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: Some(management.clone()),
        profile_id: "oauth-import-test".into(),
        default_model: "unused".into(),
        providers,
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases,
        refresh_fences,
    });
    (actor, snapshot, management)
}

fn start_oauth_import_heal_test_actor(
    store: &SqliteStoreHandle,
    vault: Arc<MemoryVault>,
    catalog: OAuthProviderCatalog,
) -> (
    AccountActorHandle,
    CredentialBroker,
    AccountsSnapshot,
    RefreshFenceRegistry,
) {
    start_oauth_import_heal_test_actor_with_native(
        store,
        vault,
        catalog,
        Arc::new(PlatformClaudeNativeCredentialStore::default()),
        None,
    )
}

fn start_oauth_import_heal_test_actor_with_native(
    store: &SqliteStoreHandle,
    vault: Arc<MemoryVault>,
    catalog: OAuthProviderCatalog,
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
    refresh_exchange: Option<Arc<dyn OAuthRefreshExchange>>,
) -> (
    AccountActorHandle,
    CredentialBroker,
    AccountsSnapshot,
    RefreshFenceRegistry,
) {
    let accounts = memory_accounts();
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let providers = test_provider_registry();
    let management = ManagementSnapshot::new(0, Vec::new(), providers.summaries(&|_| false));
    let refresh_fences = RefreshFenceRegistry::default();
    let broker_vault = vault.clone() as Arc<dyn Vault>;
    let broker_snapshot = Arc::clone(&snapshot);
    let broker_fences = refresh_fences.clone();
    let (actor, broker) = start_account_actor_with_services(
        AccountActorConfig {
            store: store.clone(),
            accounts,
            vault: vault as Arc<dyn Vault>,
            validator: Arc::new(ProviderCredentialValidator),
            snapshot: Arc::clone(&snapshot),
            management: Some(management),
            profile_id: "oauth-import-heal-test".into(),
            default_model: "unused".into(),
            providers,
            provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
            reserved_aliases: HashSet::new(),
            refresh_fences: refresh_fences.clone(),
        },
        |commands| {
            let broker = CredentialBroker::new_with_fences(
                broker_vault,
                catalog,
                broker_snapshot,
                commands,
                broker_fences,
            )?;
            match refresh_exchange {
                Some(exchange) => broker.with_refresh_exchange(exchange),
                None => Ok(broker),
            }
        },
        Arc::new(ProductionProviderModelDiscoverer),
        Arc::new(UnreachableGcloud),
        claude_native,
    )
    .expect("OAuth import heal actor");
    (actor, broker, snapshot, refresh_fences)
}

/// Always reports the R10 "validation unavailable" arm — the retryable
/// failure that leaves the secret with the COMMAND — and counts its calls,
/// so a reused expired secret is visible as a second validation.
#[derive(Default)]
struct UnavailableValidator {
    calls: std::sync::atomic::AtomicUsize,
}

impl UnavailableValidator {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl CredentialValidator for UnavailableValidator {
    fn supports(&self, provider: &str) -> bool {
        provider == "anthropic"
    }

    async fn validate(
        &self,
        _provider: &str,
        _model: &str,
        _secret: &[u8],
        _endpoint: Option<&str>,
    ) -> Result<ValidatedIdentity, ValidationError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ValidationError {
            kind: ValidationFailureKind::Unavailable,
            message: "credential validation reported Overloaded".into(),
        })
    }
}

fn login_job(
    command_id: &str,
    request_id: &str,
    secret: Option<&[u8]>,
    sink: &Arc<dyn FrameSink>,
) -> LoginJob {
    LoginJob {
        command_id: command_id.to_owned(),
        provider: "anthropic".into(),
        display_alias: Some("work".into()),
        validation_model: Some("claude-test".into()),
        secret: secret.map(|bytes| Zeroizing::new(bytes.to_vec())),
        route: LoginRoute {
            request_id: RequestId::new(request_id),
            sink: Arc::clone(sink),
        },
    }
}

fn expect_error(frame: WireFrame) -> (String, bool) {
    match frame {
        WireFrame::Response {
            body: ResponseBody::Error {
                code, retryable, ..
            },
            ..
        } => (code, retryable),
        other => panic!("expected a correlated error, got {other:?}"),
    }
}

/// A failed receipt/revision commit cannot publish externally mutated account
/// state under the prior management revision.
///
/// MUTATION CHECK: move `publish_management_snapshot` above
/// `finalize_login_receipt` in `finalize_and_respond`. Expected runtime
/// failure: after the injected store close, the descriptor appears in both
/// published snapshots even though the response is an error and revision is
/// still zero.
#[tokio::test]
async fn login_publishes_only_after_receipt_and_revision_commit() {
    struct HeldSuccessValidator {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl CredentialValidator for HeldSuccessValidator {
        fn supports(&self, provider: &str) -> bool {
            provider == "anthropic"
        }

        async fn validate(
            &self,
            _provider: &str,
            _model: &str,
            _secret: &[u8],
            _endpoint: Option<&str>,
        ) -> Result<ValidatedIdentity, ValidationError> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(ValidatedIdentity {
                identity: "held success".into(),
            })
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let management = ManagementSnapshot::new(0, Vec::new(), Vec::new());
    let vault = Arc::new(MemoryVault::new());
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(HeldSuccessValidator {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
        snapshot: Arc::clone(&snapshot),
        management: Some(management.clone()),
        profile_id: "publish-order".into(),
        default_model: "claude-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let (sink, mut frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::Login(Box::new(login_job(
            "publish-order-command",
            "publish-order-request",
            Some(b"publish-order-secret"),
            &sink,
        ))))
        .await
        .expect("send login");

    entered.notified().await;
    store.clone().close().await.expect("close receipt store");
    release.notify_one();
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("response deadline")
        .expect("response");
    let (code, retryable) = expect_error(frame);
    assert_eq!(code, ERROR_CODE_PROVIDER_ERROR);
    assert!(retryable);
    assert!(
        !vault.list().expect("vault list").is_empty(),
        "external vault mutation must have succeeded before finalization failed"
    );
    assert!(snapshot.lock().expect("resolver snapshot").is_empty());
    let view = management.read().expect("management snapshot");
    assert_eq!(view.revision, 0);
    assert!(view.descriptors.is_empty());

    actor.shutdown().await;
}

// MUTATION CHECK (R7/R10 pending-command secret TTL): disable BOTH
// enforcement sites — `handle_login`'s claim guard
// `Some(entry) if entry.claimed_at.elapsed() < SECRET_TTL` and the actor
// loop's `pending.retain(|_, entry| entry.claimed_at.elapsed() < SECRET_TTL)`
// — by replacing each condition with `true`. Expected failure: the expired
// secret is reused, so the stage-less retry validates a SECOND time and
// answers `provider_error` instead of `restage_required`, the pending map is
// not empty at the assertion, and the validator call count is 2.
#[tokio::test(start_paused = true)]
async fn pending_login_secret_past_the_ttl_is_wiped_and_forces_restage() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let mut accounts = memory_accounts();
    let vault = MemoryVault::default();
    let validator = UnavailableValidator::default();
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let mut pending: HashMap<String, PendingSecret> = HashMap::new();
    let (sink, mut frames) = channel_sink();

    // Retryable validation failure: the COMMAND keeps the claimed secret so
    // the same command can retry without retyping the key.
    handle_login(
        &store,
        &mut accounts,
        &vault,
        &validator,
        &snapshot,
        None,
        &login_registry(),
        "profile-ttl",
        "claude-test",
        &mut pending,
        &HashSet::new(),
        login_job("command-ttl", "req-1", Some(b"sk-retained"), &sink),
    )
    .await;
    let (code, retryable) = expect_error(
        frames
            .try_recv()
            .unwrap_or_else(|error| panic!("first login response: {error}")),
    );
    assert_eq!(code, ERROR_CODE_PROVIDER_ERROR);
    assert!(retryable, "validation unavailability must be retryable");
    assert!(
        pending.contains_key("command-ttl"),
        "a retryable validation must retain the command-owned secret"
    );
    assert_eq!(validator.calls(), 1);

    tokio::time::advance(SECRET_TTL + Duration::from_secs(1)).await;

    // Stage-less retry of the SAME command, now past the TTL.
    handle_login(
        &store,
        &mut accounts,
        &vault,
        &validator,
        &snapshot,
        None,
        &login_registry(),
        "profile-ttl",
        "claude-test",
        &mut pending,
        &HashSet::new(),
        login_job("command-ttl", "req-2", None, &sink),
    )
    .await;
    let (code, retryable) = expect_error(
        frames
            .try_recv()
            .unwrap_or_else(|error| panic!("retry response: {error}")),
    );
    // The WIRE literal, not just the constant (see the haider-rpc golden
    // `account_and_vault_stable_codes_pin_their_wire_literals`).
    assert_eq!(
        code, "restage_required",
        "an expired command-owned secret must force an explicit restage"
    );
    assert_eq!(code, ERROR_CODE_RESTAGE_REQUIRED);
    assert!(retryable, "restage_required is retryable once re-staged");

    // WIPED, not merely unreachable: the entry is gone from the pending map,
    // dropping (and zeroizing) the secret with it.
    assert!(
        pending.is_empty(),
        "an expired pending secret must be wiped, not parked"
    );
    assert_eq!(
        validator.calls(),
        1,
        "an expired secret must never reach the validator again"
    );

    // Neither leg persisted anything.
    assert!(
        vault
            .list()
            .unwrap_or_else(|error| panic!("{error:?}"))
            .is_empty(),
        "no vault entries"
    );
    assert!(accounts.list().is_empty(), "no descriptors");

    store
        .close()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
}

// MUTATION CHECK (same two TTL sites, driven through the ACTOR loop): with
// both conditions replaced by `true` the expired secret survives the actor's
// pre-command sweep and is handed to validation again, so the second
// response below is `provider_error`, not `restage_required`.
#[tokio::test(start_paused = true)]
async fn account_actor_answers_restage_required_after_the_pending_ttl() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let validator = Arc::new(UnavailableValidator::default());
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: Arc::new(MemoryVault::default()) as Arc<dyn Vault>,
        validator: Arc::clone(&validator) as Arc<dyn CredentialValidator>,
        snapshot,
        management: None,
        profile_id: "profile-ttl-actor".into(),
        default_model: "claude-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();

    commands
        .send(AccountCommand::Login(Box::new(login_job(
            "command-actor-ttl",
            "req-actor-1",
            Some(b"sk-retained"),
            &sink,
        ))))
        .await
        .unwrap_or_else(|_| panic!("actor admits the first login"));
    let (code, retryable) = expect_error(
        frames
            .recv()
            .await
            .unwrap_or_else(|| panic!("first login response")),
    );
    assert_eq!(code, ERROR_CODE_PROVIDER_ERROR);
    assert!(retryable);

    tokio::time::advance(SECRET_TTL + Duration::from_secs(1)).await;

    commands
        .send(AccountCommand::Login(Box::new(login_job(
            "command-actor-ttl",
            "req-actor-2",
            None,
            &sink,
        ))))
        .await
        .unwrap_or_else(|_| panic!("actor admits the stage-less retry"));
    let (code, retryable) = expect_error(
        frames
            .recv()
            .await
            .unwrap_or_else(|| panic!("retry response")),
    );
    assert_eq!(
        code, "restage_required",
        "the actor must not hand an expired secret back to validation"
    );
    assert!(retryable);
    assert_eq!(
        validator.calls(),
        1,
        "an expired secret must never reach the validator again"
    );

    actor.shutdown().await;
    store
        .close()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
}

/// Receipt-shape laws for the login command (the first non-wire receipt
/// caller): fresh claim, pending resume, committed replay, digest mismatch,
/// failed-terminal replay, and the reconciliation scan.
#[tokio::test]
async fn login_receipt_claims_replay_and_reject_like_every_r2_command() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let identity = identity_for("profile-a", "command-1");
    let json = identity
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest = blake3::hash(json.as_bytes()).to_hex().to_string();

    // Fresh claim, then pending resume on re-claim.
    let claim = store
        .login_claim_receipt("command-1".into(), digest.clone(), json.clone())
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(claim, LoginClaim::Fresh);
    let claim = store
        .login_claim_receipt("command-1".into(), digest.clone(), json.clone())
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(claim, LoginClaim::ResumePending);

    // Same command with a DIFFERENT semantic body is rejected (R7: changing
    // provider/resolved-model/alias under a reused command id).
    let other = LoginIdentity {
        provider: "anthropic".into(),
        resolved_model: "claude-other".into(),
        display_alias: None,
        physical_alias: identity.physical_alias.clone(),
    };
    let other_json = other
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let other_digest = blake3::hash(other_json.as_bytes()).to_hex().to_string();
    let mismatch = store
        .login_claim_receipt("command-1".into(), other_digest, other_json)
        .await
        .expect_err("different body under a reused command id must be rejected");
    assert!(mismatch.message.contains("different method or semantic"));

    // The scan sees the pending row.
    let rows = store
        .login_receipts()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "pending");

    // Finalize commits the descriptor response; a replay returns it.
    let descriptor = CredentialDescriptor {
        alias: CredentialAlias::new(identity.physical_alias.clone()),
        provider: "anthropic".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "work".into(),
        status: CredentialStatus::Ok,
        active: true,
    };
    store
        .finalize_login_receipt(
            "command-1".into(),
            LoginReceiptResponse {
                descriptor: descriptor.clone(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    let claim = store
        .login_claim_receipt("command-1".into(), digest, json)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    match claim {
        LoginClaim::Committed(response) => {
            assert_eq!(response.descriptor, descriptor);
        }
        other => panic!("expected committed replay, got {other:?}"),
    }

    // A definitive failure records terminally and a replay is a typed error.
    let identity_2 = identity_for("profile-a", "command-2");
    let json_2 = identity_2
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_2 = blake3::hash(json_2.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-2".into(), digest_2.clone(), json_2.clone())
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    store
        .fail_login_receipt(
            "command-2".into(),
            LoginReceiptFailure {
                code: "unauthorized".into(),
                message: "credential validation reported Authentication".into(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    let replay = store
        .login_claim_receipt("command-2".into(), digest_2, json_2)
        .await
        .expect_err("failed login receipts are terminal");
    assert!(replay.message.contains("already recorded as failed"));

    // Failed receipts are excluded from the reconciliation scan.
    let rows = store
        .login_receipts()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(
        rows.len(),
        1,
        "only the committed receipt remains scannable"
    );
    assert_eq!(rows[0].state, "committed");

    store
        .close()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
}

/// Pre-ready reconciliation allocates the missing final revision once after
/// durable descriptor state proves the external mutation succeeded.
///
/// MUTATION CHECK: remove the `finalize_reconciled` call from the
/// pending-plus-descriptor branch of `reconcile_login_receipts`. Expected
/// runtime failure: the first reconciliation leaves revision zero and the
/// receipt pending instead of committing revision one.
#[tokio::test]
async fn login_reconciliation_advances_a_missing_revision_exactly_once() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let identity = identity_for("reconcile-revision", "reconcile-command");
    let request_json = identity.canonical_json().expect("request JSON");
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    store
        .login_claim_receipt("reconcile-command".into(), request_digest, request_json)
        .await
        .expect("claim receipt");

    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let mut accounts = memory_accounts();
    accounts
        .add(descriptor_for(&identity, &alias, Some("reconciled".into())))
        .expect("persist descriptor");
    let vault = VaultProvision::Available(Arc::new(MemoryVault::new()) as Arc<dyn Vault>);

    reconcile_login_receipts(&store, &mut accounts, &vault)
        .await
        .expect("first reconciliation");
    assert_eq!(store.management_revision().await.expect("revision"), 1);
    let rows = store.login_receipts().await.expect("receipt rows");
    assert_eq!(rows[0].state, "committed");
    assert_eq!(rows[0].final_revision, Some(1));

    reconcile_login_receipts(&store, &mut accounts, &vault)
        .await
        .expect("second reconciliation");
    assert_eq!(
        store.management_revision().await.expect("stable revision"),
        1
    );
}

// MUTATION CHECK (R10 step 10): make `reconcile_login_receipts` skip the
// vault-only arm (treat pending+vault as "neither"). Expected failure: the
// vault-only boundary below never produces a descriptor and its receipt
// stays pending.
#[tokio::test]
async fn reconciliation_closes_every_login_crash_boundary() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let vault = Arc::new(MemoryVault::default());
    let provision = VaultProvision::Available(vault.clone() as Arc<dyn Vault>);
    let mut accounts = memory_accounts();

    // Boundary A — claimed, nothing else (crash before vault write): stays
    // pending, waiting for the same command with a fresh stage.
    let identity_a = identity_for("profile-r", "command-a");
    let json_a = identity_a
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_a = blake3::hash(json_a.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-a".into(), digest_a, json_a)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));

    // Boundary B — vault written, descriptor missing (crash between
    // Keychain and descriptor save): resume the descriptor commit and
    // finalize.
    let identity_b = identity_for("profile-r", "command-b");
    let json_b = identity_b
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_b = blake3::hash(json_b.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-b".into(), digest_b, json_b)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    vault
        .put(
            &CredentialAlias::new(identity_b.physical_alias.clone()),
            b"sk-vault-only",
        )
        .unwrap_or_else(|error| panic!("{error:?}"));

    // Boundary C — vault + descriptor, receipt still pending (crash before
    // finalization): finalize the committed response.
    let identity_c = identity_for("profile-r", "command-c");
    let json_c = identity_c
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_c = blake3::hash(json_c.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-c".into(), digest_c, json_c)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    vault
        .put(
            &CredentialAlias::new(identity_c.physical_alias.clone()),
            b"sk-both",
        )
        .unwrap_or_else(|error| panic!("{error:?}"));
    accounts
        .add(descriptor_for(
            &identity_c,
            &CredentialAlias::new(identity_c.physical_alias.clone()),
            None,
        ))
        .unwrap_or_else(|error| panic!("{error:?}"));

    // Boundary D — committed receipt but the descriptor file lost the row
    // (descriptor-store rollback after the fact): self-heal from the
    // durable response without stealing active.
    let identity_d = identity_for("profile-r", "command-d");
    let json_d = identity_d
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_d = blake3::hash(json_d.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-d".into(), digest_d, json_d)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    let descriptor_d = CredentialDescriptor {
        alias: CredentialAlias::new(identity_d.physical_alias.clone()),
        provider: "anthropic".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "healed".into(),
        status: CredentialStatus::Ok,
        active: true,
    };
    store
        .finalize_login_receipt(
            "command-d".into(),
            LoginReceiptResponse {
                descriptor: descriptor_d.clone(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));

    reconcile_login_receipts(&store, &mut accounts, &provision)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));

    // A: still pending (fresh-stage retry path).
    let rows = store
        .login_receipts()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    let state_of = |command: &str| {
        rows.iter()
            .find(|row| row.command_id == command)
            .map(|row| row.state.clone())
            .unwrap_or_else(|| panic!("receipt {command} missing"))
    };
    assert_eq!(state_of("command-a"), "pending");
    assert_eq!(state_of("command-b"), "committed");
    assert_eq!(state_of("command-c"), "committed");
    assert_eq!(state_of("command-d"), "committed");

    // B and C produced descriptors; D self-healed WITHOUT stealing the
    // provider's active slot (C's earlier active survives).
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_b.physical_alias.clone()))
            .is_some()
    );
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_d.physical_alias.clone()))
            .is_some_and(|descriptor| !descriptor.active)
    );
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_a.physical_alias.clone()))
            .is_none(),
        "boundary A must not fabricate a descriptor"
    );

    store
        .close()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
}

#[tokio::test]
async fn oauth_account_add_receipt_is_distinct_idempotent_and_secret_free() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let identity = oauth_identity_for("profile-o", "oauth-command");
    let request_json = identity.canonical_json().expect("canonical OAuth identity");
    let digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    for secret in [
        "OAUTH_ACCESS_RECEIPT_SENTINEL_1fa2",
        "OAUTH_REFRESH_RECEIPT_SENTINEL_9c31",
        "oauth-ready-ref-must-not-persist",
    ] {
        assert!(!request_json.contains(secret));
    }
    assert_eq!(
        store
            .account_add_claim_receipt(
                "oauth-command".into(),
                digest.clone(),
                request_json.clone(),
            )
            .await
            .expect("fresh"),
        AccountAddClaim::Fresh
    );
    assert_eq!(
        store
            .account_add_claim_receipt(
                "oauth-command".into(),
                digest.clone(),
                request_json.clone(),
            )
            .await
            .expect("resume"),
        AccountAddClaim::ResumePending
    );
    let descriptor = oauth_descriptor(
        &identity,
        &CredentialAlias::new(identity.physical_alias.clone()),
        &oauth_bundle(),
    );
    store
        .finalize_account_add_receipt(
            "oauth-command".into(),
            AccountAddReceiptResponse {
                descriptor: descriptor.clone(),
            },
        )
        .await
        .expect("finalize");
    assert!(matches!(
        store
            .account_add_claim_receipt("oauth-command".into(), digest, request_json)
            .await
            .expect("replay"),
        AccountAddClaim::Committed(response) if response.descriptor == descriptor
    ));

    // The method is deliberately not aliased to account.login_api.
    let login_identity = identity_for("profile-o", "oauth-command");
    let login_json = login_identity.canonical_json().expect("login identity");
    let login_digest = blake3::hash(login_json.as_bytes()).to_hex().to_string();
    let collision = store
        .login_claim_receipt("oauth-command".into(), login_digest, login_json)
        .await
        .expect_err("different durable methods cannot share a command id");
    assert!(collision.message.contains("different method or semantic"));

    store.close().await.expect("close");
}

#[tokio::test]
async fn oauth_account_add_never_exposes_initial_token_before_vault_persistence() {
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct BlockingPutVault {
        inner: MemoryVault,
        entered: AtomicBool,
        release: (StdMutex<bool>, Condvar),
    }

    impl Vault for BlockingPutVault {
        fn put(
            &self,
            alias: &CredentialAlias,
            secret: &[u8],
        ) -> haider_accounts::AccountsResult<()> {
            self.entered.store(true, Ordering::SeqCst);
            let (lock, wake) = &self.release;
            let mut released = lock.lock().expect("release lock");
            while !*released {
                released = wake.wait(released).expect("release wait");
            }
            self.inner.put(alias, secret)
        }

        fn resolve(
            &self,
            alias: &CredentialAlias,
        ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
            self.inner.resolve(alias)
        }

        fn delete(&self, alias: &CredentialAlias) -> haider_accounts::AccountsResult<()> {
            self.inner.delete(alias)
        }

        fn list(&self) -> haider_accounts::AccountsResult<Vec<CredentialAlias>> {
            self.inner.list()
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let vault = Arc::new(BlockingPutVault {
        inner: MemoryVault::new(),
        entered: AtomicBool::new(false),
        release: (StdMutex::new(false), Condvar::new()),
    });
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "profile-initial-persist".into(),
        default_model: "unused".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let (sink, mut frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::AddOAuth(Box::new(OAuthAddJob {
            command_id: "oauth-initial-persist".into(),
            provider: "fake-oauth".into(),
            display_alias: "work-oauth".into(),
            claim: Some(OAuthReadyClaim::for_account_test(
                "fake-oauth",
                "work-oauth",
                oauth_bundle(),
            )),
            route: LoginRoute {
                request_id: RequestId::new("oauth-initial-persist-request"),
                sink,
            },
        })))
        .await
        .expect("actor command");
    tokio::time::timeout(Duration::from_secs(1), async {
        while !vault.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("vault put entered");
    let alias = CredentialAlias::new("work-oauth");
    assert!(frames.try_recv().is_err(), "no response before vault put");
    assert!(snapshot.lock().expect("snapshot").is_empty());
    assert!(
        vault.resolve(&alias).is_err(),
        "token is not yet resolvable"
    );
    {
        let (release, wake) = &vault.release;
        *release.lock().expect("release") = true;
        wake.notify_all();
    }
    let frame = tokio::time::timeout(Duration::from_secs(1), frames.recv())
        .await
        .expect("account response")
        .expect("account frame");
    assert!(matches!(
        frame,
        WireFrame::Response {
            body: ResponseBody::AccountAdd { .. },
            ..
        }
    ));
    assert!(vault.resolve(&alias).is_ok());
    assert_eq!(snapshot.lock().expect("snapshot").len(), 1);
    actor.shutdown().await;
    store.close().await.expect("close");
}

#[tokio::test]
async fn oauth_account_add_actor_crash_after_vault_put_reconciles_production_receipt() {
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct PutThenBlockVault {
        inner: MemoryVault,
        persisted: AtomicBool,
        release: (StdMutex<bool>, Condvar),
    }

    impl Vault for PutThenBlockVault {
        fn put(
            &self,
            alias: &CredentialAlias,
            secret: &[u8],
        ) -> haider_accounts::AccountsResult<()> {
            self.inner.put(alias, secret)?;
            self.persisted.store(true, Ordering::SeqCst);
            let (lock, wake) = &self.release;
            let mut released = lock.lock().expect("release lock");
            while !*released {
                released = wake.wait(released).expect("release wait");
            }
            Ok(())
        }

        fn resolve(
            &self,
            alias: &CredentialAlias,
        ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
            self.inner.resolve(alias)
        }

        fn delete(&self, alias: &CredentialAlias) -> haider_accounts::AccountsResult<()> {
            self.inner.delete(alias)
        }

        fn list(&self) -> haider_accounts::AccountsResult<Vec<CredentialAlias>> {
            self.inner.list()
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let vault = Arc::new(PutThenBlockVault {
        inner: MemoryVault::new(),
        persisted: AtomicBool::new(false),
        release: (StdMutex::new(false), Condvar::new()),
    });
    let actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "profile-oauth-crash".into(),
        default_model: "unused".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let (sink, mut frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::AddOAuth(Box::new(OAuthAddJob {
            command_id: "oauth-crash-command".into(),
            provider: "fake-oauth".into(),
            display_alias: "work-oauth".into(),
            claim: Some(OAuthReadyClaim::for_account_test(
                "fake-oauth",
                "work-oauth",
                oauth_bundle(),
            )),
            route: LoginRoute {
                request_id: RequestId::new("oauth-crash-request"),
                sink,
            },
        })))
        .await
        .expect("actor command");
    tokio::time::timeout(Duration::from_secs(1), async {
        while !vault.persisted.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("vault persisted before crash");

    // Crash at the production seam: SQLite receipt is pending and the vault
    // write is durable, but the actor has not observed the worker return, so
    // neither descriptor nor response can have been published.
    actor.crash();
    assert!(snapshot.lock().expect("snapshot").is_empty());
    assert!(frames.try_recv().is_err());
    {
        let (release, wake) = &vault.release;
        *release.lock().expect("release") = true;
        wake.notify_all();
    }
    let alias = CredentialAlias::new("work-oauth");
    tokio::time::timeout(Duration::from_secs(1), async {
        while vault.resolve(&alias).is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable vault value");

    let mut restarted_accounts = memory_accounts();
    reconcile_oauth_add_receipts(
        &store,
        &mut restarted_accounts,
        &VaultProvision::Available(vault.clone() as Arc<dyn Vault>),
    )
    .await
    .expect("restart reconciliation");
    assert!(
        restarted_accounts.get(&alias).is_some(),
        "restart reconstructs the descriptor from the durable bundle"
    );
    let receipts = store.account_add_receipts().await.expect("receipt rows");
    assert!(receipts.iter().any(|receipt| {
        receipt.command_id == "oauth-crash-command" && receipt.state == "committed"
    }));
    store.close().await.expect("close");
}

#[tokio::test]
async fn oauth_reconciliation_closes_crash_before_and_after_vault_put() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let provision = VaultProvision::Available(vault.clone() as Arc<dyn Vault>);
    let mut accounts = memory_accounts();

    // A: receipt claimed, crash before vault put. Browser/ready objects are
    // gone after restart, so this stays pending for a fresh flow.
    let identity_a = oauth_identity_for("profile-r", "oauth-a");
    let json_a = identity_a.canonical_json().expect("identity A");
    let digest_a = blake3::hash(json_a.as_bytes()).to_hex().to_string();
    assert_eq!(
        store
            .account_add_claim_receipt("oauth-a".into(), digest_a, json_a)
            .await
            .expect("claim A"),
        AccountAddClaim::Fresh
    );

    // B: vault bundle durable, crash before descriptor save. Reconciliation
    // verifies the versioned bundle, creates the OAuth descriptor, finalizes.
    let identity_b = oauth_identity_for("profile-r", "oauth-b");
    let json_b = identity_b.canonical_json().expect("identity B");
    let digest_b = blake3::hash(json_b.as_bytes()).to_hex().to_string();
    let _ = store
        .account_add_claim_receipt("oauth-b".into(), digest_b, json_b)
        .await
        .expect("claim B");
    vault
        .put(
            &CredentialAlias::new(identity_b.physical_alias.clone()),
            &oauth_bundle().encode().expect("encode B"),
        )
        .expect("vault B");

    // C: vault + descriptor committed, crash before receipt finalization.
    let identity_c = oauth_identity_for("profile-r", "oauth-c");
    let json_c = identity_c.canonical_json().expect("identity C");
    let digest_c = blake3::hash(json_c.as_bytes()).to_hex().to_string();
    let _ = store
        .account_add_claim_receipt("oauth-c".into(), digest_c, json_c)
        .await
        .expect("claim C");
    let bundle_c = oauth_bundle();
    vault
        .put(
            &CredentialAlias::new(identity_c.physical_alias.clone()),
            &bundle_c.encode().expect("encode C"),
        )
        .expect("vault C");
    accounts
        .add(oauth_descriptor(
            &identity_c,
            &CredentialAlias::new(identity_c.physical_alias.clone()),
            &bundle_c,
        ))
        .expect("descriptor C");

    // D: committed receipt survives a lost descriptor and self-heals without
    // stealing the active slot from the later credential.
    let identity_d = oauth_identity_for("profile-r", "oauth-d");
    let json_d = identity_d.canonical_json().expect("identity D");
    let digest_d = blake3::hash(json_d.as_bytes()).to_hex().to_string();
    let _ = store
        .account_add_claim_receipt("oauth-d".into(), digest_d, json_d)
        .await
        .expect("claim D");
    let mut descriptor_d = oauth_descriptor(
        &identity_d,
        &CredentialAlias::new(identity_d.physical_alias.clone()),
        &oauth_bundle(),
    );
    descriptor_d.active = true;
    store
        .finalize_account_add_receipt(
            "oauth-d".into(),
            AccountAddReceiptResponse {
                descriptor: descriptor_d.clone(),
            },
        )
        .await
        .expect("finalize D");

    reconcile_oauth_add_receipts(&store, &mut accounts, &provision)
        .await
        .expect("OAuth reconciliation");
    let rows = store.account_add_receipts().await.expect("receipt rows");
    let state = |command: &str| {
        rows.iter()
            .find(|row| row.command_id == command)
            .map(|row| row.state.as_str())
            .unwrap_or("missing")
    };
    assert_eq!(state("oauth-a"), "pending");
    assert_eq!(state("oauth-b"), "committed");
    assert_eq!(state("oauth-c"), "committed");
    assert_eq!(state("oauth-d"), "committed");
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_a.physical_alias))
            .is_none()
    );
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_b.physical_alias))
            .is_some()
    );
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_d.physical_alias))
            .is_some_and(|descriptor| !descriptor.active)
    );

    store.close().await.expect("close");
}

#[tokio::test]
async fn refresh_cas_ignores_benign_status_and_selection_changes() {
    let revision_dir = test_store_dir();
    let revision_store = open_store(revision_dir.path()).await;
    let mut accounts = memory_accounts();
    let identity = oauth_identity_for("profile-cas", "oauth-cas");
    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let original = oauth_bundle();
    let captured = oauth_descriptor(&identity, &alias, &original);
    accounts
        .add(captured.clone())
        .expect("add OAuth descriptor");
    let other_alias = CredentialAlias::new("other-oauth-account");
    accounts
        .add(CredentialDescriptor {
            alias: other_alias.clone(),
            provider: captured.provider.clone(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: "other@example.invalid".into(),
            status: CredentialStatus::Ok,
            active: false,
        })
        .expect("add other descriptor");
    accounts
        .set_status(&alias, CredentialStatus::Limited { until_ms: 123_456 })
        .expect("limit captured descriptor");
    accounts
        .select(&other_alias)
        .expect("change active selection");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(&alias, &original.encode().expect("encode original"))
        .expect("seed vault");
    let rotated = haider_accounts::OAuthTokenBundleV1::new(
        original.provider_id.clone(),
        original.issuer.clone(),
        original.audience.clone(),
        original.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"ROTATED_CAS_ACCESS_58af".to_vec()),
        Some(Zeroizing::new(b"ROTATED_CAS_REFRESH_914b".to_vec())),
        u64::MAX - 2,
        Some(u64::MAX),
        original.granted_scopes.clone(),
        original.identity.clone(),
        2,
    )
    .expect("rotated bundle");
    apply_oauth_refresh(
        &mut accounts,
        vault.clone() as Arc<dyn Vault>,
        &snapshot,
        None,
        &revision_store,
        &captured,
        &OAuthRefreshFence {
            fence_epoch: 0,
            generation: 1,
            issuer: original.issuer.clone(),
            audience: original.audience.clone(),
            resource: original.resource.clone(),
            subject_hash: original.identity.subject_hash.clone(),
        },
        rotated.encode().expect("encode rotated"),
        &RefreshFenceRegistry::default(),
        &watch::channel(false).1,
    )
    .await
    .expect("benign fields do not fence rotation");
    let current = accounts.get(&alias).expect("descriptor remains");
    assert!(!current.active);
    assert!(matches!(
        current.status,
        CredentialStatus::Limited { until_ms: 123_456 }
    ));
    let stored = vault.resolve(&alias).expect("rotated vault");
    assert_eq!(
        haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
            .expect("decode rotated")
            .generation,
        2
    );
}

#[tokio::test]
async fn production_expiry_sink_rejects_a_late_failure_for_a_newer_generation() {
    let revision_dir = test_store_dir();
    let revision_store = open_store(revision_dir.path()).await;
    let mut accounts = memory_accounts();
    let identity = oauth_identity_for("profile-expiry-fence", "oauth-fence");
    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let original = oauth_bundle();
    let captured = oauth_descriptor(&identity, &alias, &original);
    accounts
        .add(captured.clone())
        .expect("add captured descriptor");
    let vault = Arc::new(MemoryVault::new());
    let replacement = haider_accounts::OAuthTokenBundleV1::new(
        original.provider_id.clone(),
        original.issuer.clone(),
        original.audience.clone(),
        original.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"NEWER_EXPIRY_FENCE_ACCESS_41c8".to_vec()),
        Some(Zeroizing::new(b"NEWER_EXPIRY_FENCE_REFRESH_911d".to_vec())),
        u64::MAX - 2,
        Some(u64::MAX),
        original.granted_scopes.clone(),
        original.identity.clone(),
        9,
    )
    .expect("replacement");
    vault
        .put(&alias, &replacement.encode().expect("encode replacement"))
        .expect("seed replacement");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let expired = expire_oauth_refresh(
        &mut accounts,
        vault.clone() as Arc<dyn Vault>,
        &snapshot,
        None,
        &revision_store,
        &captured,
        &OAuthRefreshFence {
            fence_epoch: 0,
            generation: 1,
            issuer: original.issuer.clone(),
            audience: original.audience.clone(),
            resource: original.resource.clone(),
            subject_hash: original.identity.subject_hash.clone(),
        },
        &RefreshFenceRegistry::default(),
    )
    .await
    .expect("expiry sink");
    assert!(!expired, "late failure must be a no-op");
    assert!(matches!(
        accounts.get(&alias).expect("replacement descriptor").status,
        CredentialStatus::Ok
    ));
    assert_eq!(
        haider_accounts::OAuthTokenBundleV1::decode(
            vault
                .resolve(&alias)
                .expect("replacement retained")
                .expose_secret()
        )
        .expect("decode replacement")
        .generation,
        9
    );
}

#[tokio::test]
async fn production_refresh_begin_durably_fences_restart_before_the_endpoint() {
    struct SharedDescriptorStore(Arc<StdMutex<Vec<CredentialDescriptor>>>);

    impl StoreLike for SharedDescriptorStore {
        fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
            Ok(self
                .0
                .lock()
                .map(|descriptors| descriptors.clone())
                .unwrap_or_default())
        }

        fn save(&self, descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
            if let Ok(mut durable) = self.0.lock() {
                *durable = descriptors.to_vec();
            }
            Ok(())
        }
    }

    let durable = Arc::new(StdMutex::new(Vec::new()));
    let revision_dir = test_store_dir();
    let revision_store = open_store(revision_dir.path()).await;
    let store: Box<dyn StoreLike> = Box::new(SharedDescriptorStore(Arc::clone(&durable)));
    let mut accounts = AccountStore::new(store).expect("accounts");
    let identity = oauth_identity_for("profile-refresh-begin", "oauth-begin");
    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let original = oauth_bundle();
    let descriptor = oauth_descriptor(&identity, &alias, &original);
    accounts.add(descriptor.clone()).expect("seed descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(&alias, &original.encode().expect("encode original"))
        .expect("seed vault");
    let begun = begin_oauth_refresh(
        &mut accounts,
        vault.clone() as Arc<dyn Vault>,
        &snapshot,
        None,
        &revision_store,
        &descriptor,
        &OAuthRefreshFence {
            fence_epoch: 0,
            generation: original.generation,
            issuer: original.issuer.clone(),
            audience: original.audience.clone(),
            resource: original.resource.clone(),
            subject_hash: original.identity.subject_hash.clone(),
        },
        &RefreshFenceRegistry::default(),
    )
    .await
    .expect("begin");
    assert!(begun);
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
    assert!(
        vault.resolve(&alias).is_ok(),
        "ordinary begin retains the original bundle for audit"
    );

    let restarted: Box<dyn StoreLike> = Box::new(SharedDescriptorStore(durable));
    let restarted = AccountStore::new(restarted).expect("restart descriptors");
    assert!(matches!(
        restarted.get(&alias).expect("descriptor").status,
        CredentialStatus::Expired
    ));
}

#[tokio::test]
async fn production_descriptor_failure_tombstones_the_dead_rotated_refresh_token() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FailingDescriptorStore {
        durable: Arc<StdMutex<Vec<CredentialDescriptor>>>,
        fail: Arc<AtomicBool>,
    }

    impl StoreLike for FailingDescriptorStore {
        fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
            Ok(self
                .durable
                .lock()
                .map(|descriptors| descriptors.clone())
                .unwrap_or_default())
        }

        fn save(&self, descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(HaiderError::new(
                    ErrorCode::ProviderError,
                    "injected production descriptor persistence failure",
                    false,
                ));
            }
            if let Ok(mut durable) = self.durable.lock() {
                *durable = descriptors.to_vec();
            }
            Ok(())
        }
    }

    struct FailingRotationVault {
        inner: MemoryVault,
        fail_put: AtomicBool,
    }

    impl Vault for FailingRotationVault {
        fn put(
            &self,
            alias: &CredentialAlias,
            secret: &[u8],
        ) -> haider_accounts::AccountsResult<()> {
            if self.fail_put.load(Ordering::SeqCst) {
                return Err(HaiderError::new(
                    ErrorCode::ProviderError,
                    "injected rotated bundle persistence failure",
                    false,
                ));
            }
            self.inner.put(alias, secret)
        }

        fn resolve(
            &self,
            alias: &CredentialAlias,
        ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
            self.inner.resolve(alias)
        }

        fn delete(&self, alias: &CredentialAlias) -> haider_accounts::AccountsResult<()> {
            self.inner.delete(alias)
        }

        fn list(&self) -> haider_accounts::AccountsResult<Vec<CredentialAlias>> {
            self.inner.list()
        }
    }

    let durable = Arc::new(StdMutex::new(Vec::new()));
    let revision_dir = test_store_dir();
    let revision_store = open_store(revision_dir.path()).await;
    let fail_descriptor = Arc::new(AtomicBool::new(false));
    let store: Box<dyn StoreLike> = Box::new(FailingDescriptorStore {
        durable: Arc::clone(&durable),
        fail: Arc::clone(&fail_descriptor),
    });
    let mut accounts = AccountStore::new(store).expect("accounts");
    let identity = oauth_identity_for("profile-durable-expiry", "oauth-refresh");
    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let original = oauth_bundle();
    let descriptor = oauth_descriptor(&identity, &alias, &original);
    accounts.add(descriptor.clone()).expect("seed descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(FailingRotationVault {
        inner: MemoryVault::new(),
        fail_put: AtomicBool::new(false),
    });
    vault
        .put(&alias, &original.encode().expect("encode original"))
        .expect("seed vault");
    vault.fail_put.store(true, Ordering::SeqCst);
    fail_descriptor.store(true, Ordering::SeqCst);

    let rotated = haider_accounts::OAuthTokenBundleV1::new(
        original.provider_id.clone(),
        original.issuer.clone(),
        original.audience.clone(),
        original.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"ROTATED_DURABILITY_ACCESS_d431".to_vec()),
        Some(Zeroizing::new(b"ROTATED_DURABILITY_REFRESH_9e2a".to_vec())),
        u64::MAX - 2,
        Some(u64::MAX),
        original.granted_scopes.clone(),
        original.identity.clone(),
        2,
    )
    .expect("rotated");
    assert!(matches!(
        apply_oauth_refresh(
            &mut accounts,
            vault.clone() as Arc<dyn Vault>,
            &snapshot,
            None,
            &revision_store,
            &descriptor,
            &OAuthRefreshFence {
                fence_epoch: 0,
                generation: 1,
                issuer: original.issuer.clone(),
                audience: original.audience.clone(),
                resource: original.resource.clone(),
                subject_hash: original.identity.subject_hash.clone(),
            },
            rotated.encode().expect("encode rotated"),
            &RefreshFenceRegistry::default(),
            &watch::channel(false).1,
        )
        .await,
        Err(RefreshApplyError::Persist)
    ));
    assert!(
        vault.resolve(&alias).is_err(),
        "descriptor persistence failure must durably delete the dead token"
    );
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
    assert!(matches!(
        durable.lock().expect("durable")[0].status,
        CredentialStatus::Ok
    ));

    // Restart sees the production descriptor save failure, but the durable
    // vault tombstone prevents any resolver from obtaining/retrying the dead
    // generation-1 refresh token.
    let restarted: Box<dyn StoreLike> = Box::new(FailingDescriptorStore {
        durable,
        fail: fail_descriptor,
    });
    let restarted = AccountStore::new(restarted).expect("restart descriptors");
    assert!(matches!(
        restarted.get(&alias).expect("descriptor").status,
        CredentialStatus::Ok
    ));
    assert!(vault.resolve(&alias).is_err());
}

/// Durable set-active/remove publication, deterministic successor selection,
/// committed replay, and the remove/re-add refresh epoch all run through the
/// production account actor.
///
/// MUTATION CHECK: remove `refresh_fences.invalidate(&alias)` from
/// `handle_remove_account`. Expected runtime failure: the late refresh below
/// returns `Ok(())` and replaces the re-added generation-one vault bundle
/// with `LATE_REMOVE_REFRESH_ACCESS_7cc1`.
#[tokio::test]
async fn durable_remove_fences_late_refresh_across_same_alias_readd() {
    let bundle = oauth_bundle();
    let primary_alias = CredentialAlias::new("oauth-primary");
    let removed_alias = CredentialAlias::new("oauth-removed");
    let descriptor = |alias: CredentialAlias, active: bool| CredentialDescriptor {
        alias,
        provider: "fake-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: bundle.identity.display_identity.clone(),
        status: CredentialStatus::Ok,
        active,
    };
    let mut accounts = memory_accounts();
    accounts
        .add(descriptor(primary_alias.clone(), true))
        .expect("primary descriptor");
    accounts
        .add(descriptor(removed_alias.clone(), false))
        .expect("secondary descriptor");

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let encoded = bundle.encode().expect("encode original bundle");
    vault
        .put(&primary_alias, &encoded)
        .expect("seed primary bundle");
    vault
        .put(&removed_alias, &encoded)
        .expect("seed removable bundle");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let management = ManagementSnapshot::new(0, accounts.list().to_vec(), Vec::new());
    let refresh_fences = RefreshFenceRegistry::default();
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: Some(management.clone()),
        profile_id: "remove-refresh-fence".into(),
        default_model: "unused".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: refresh_fences.clone(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();

    commands
        .send(AccountCommand::SetActive(Box::new(SetActiveJob {
            command_id: "set-active-before-remove".into(),
            alias: removed_alias.as_str().into(),
            route: LoginRoute {
                request_id: RequestId::new("set-active-before-remove-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("send set-active");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("set-active deadline")
        .expect("set-active response")
    {
        WireFrame::Response {
            body:
                ResponseBody::AccountSetActive {
                    descriptor,
                    prior_alias,
                    revision,
                },
            ..
        } => {
            assert_eq!(descriptor.alias, removed_alias);
            assert_eq!(prior_alias, Some(primary_alias.clone()));
            assert_eq!(revision, 1);
        }
        other => panic!("unexpected set-active response: {other:?}"),
    }

    commands
        .send(AccountCommand::Remove(Box::new(RemoveAccountJob {
            command_id: "remove-active-oauth".into(),
            alias: removed_alias.as_str().into(),
            expected_revision: Some(1),
            route: LoginRoute {
                request_id: RequestId::new("remove-active-oauth-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("send remove");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("remove deadline")
        .expect("remove response")
    {
        WireFrame::Response {
            body:
                ResponseBody::AccountRemove {
                    removed_alias: response_alias,
                    replacement_active_alias,
                    revision,
                },
            ..
        } => {
            assert_eq!(response_alias, removed_alias);
            assert_eq!(replacement_active_alias, Some(primary_alias.clone()));
            assert_eq!(revision, 2);
        }
        other => panic!("unexpected remove response: {other:?}"),
    }
    assert!(vault.resolve(&removed_alias).is_err());
    assert!(
        store
            .reserved_account_aliases()
            .await
            .expect("reservations")
            .is_empty()
    );
    let after_remove = management.read().expect("management snapshot");
    assert_eq!(after_remove.revision, 2);
    assert_eq!(after_remove.descriptors.len(), 1);
    assert_eq!(after_remove.descriptors[0].alias, primary_alias);
    assert!(after_remove.descriptors[0].active);
    drop(after_remove);

    commands
        .send(AccountCommand::AddOAuth(Box::new(OAuthAddJob {
            command_id: "readd-removed-oauth".into(),
            provider: "fake-oauth".into(),
            display_alias: removed_alias.as_str().into(),
            claim: Some(OAuthReadyClaim::for_account_test(
                "fake-oauth",
                removed_alias.as_str(),
                oauth_bundle(),
            )),
            route: LoginRoute {
                request_id: RequestId::new("readd-removed-oauth-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("send re-add");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("re-add deadline")
            .expect("re-add response"),
        WireFrame::Response {
            body: ResponseBody::AccountAdd { .. },
            ..
        }
    ));

    let late = haider_accounts::OAuthTokenBundleV1::new(
        bundle.provider_id.clone(),
        bundle.issuer.clone(),
        bundle.audience.clone(),
        bundle.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"LATE_REMOVE_REFRESH_ACCESS_7cc1".to_vec()),
        Some(Zeroizing::new(b"LATE_REMOVE_REFRESH_REFRESH_91ea".to_vec())),
        u64::MAX - 2,
        Some(u64::MAX),
        bundle.granted_scopes.clone(),
        bundle.identity.clone(),
        2,
    )
    .expect("late bundle");
    let (completed, result) = tokio::sync::oneshot::channel();
    commands
        .send(AccountCommand::ApplyOAuthRefresh {
            descriptor: descriptor(removed_alias.clone(), true),
            expected: OAuthRefreshFence {
                fence_epoch: 0,
                generation: bundle.generation,
                issuer: bundle.issuer.clone(),
                audience: bundle.audience.clone(),
                resource: bundle.resource.clone(),
                subject_hash: bundle.identity.subject_hash.clone(),
            },
            encoded_bundle: late.encode().expect("encode late bundle"),
            completed,
        })
        .await
        .expect("send late refresh");
    assert!(matches!(
        result.await.expect("late refresh completion"),
        Err(RefreshApplyError::Stale)
    ));
    let retained = haider_accounts::OAuthTokenBundleV1::decode(
        vault
            .resolve(&removed_alias)
            .expect("re-added bundle retained")
            .expose_secret(),
    )
    .expect("decode retained bundle");
    assert_eq!(retained.generation, 1);

    commands
        .send(AccountCommand::SetActive(Box::new(SetActiveJob {
            command_id: "set-active-before-remove".into(),
            alias: removed_alias.as_str().into(),
            route: LoginRoute {
                request_id: RequestId::new("set-active-replay-request"),
                sink,
            },
        })))
        .await
        .expect("send set-active replay");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("replay deadline")
        .expect("replay response")
    {
        WireFrame::Response {
            body: ResponseBody::AccountSetActive { revision, .. },
            ..
        } => assert_eq!(revision, 1),
        other => panic!("unexpected replay response: {other:?}"),
    }

    actor.shutdown().await;
}

/// An omitted API alias is command-derived before secret validation and is
/// retained in the durable semantic identity for replay.
///
/// MUTATION CHECK: hash `provider.as_bytes()` instead of
/// `command_id.as_bytes()` in `canonical_api_alias`. Expected runtime
/// failure: the response alias differs from the independently command-derived
/// alias asserted below.
#[tokio::test]
async fn omitted_api_alias_is_stable_command_derived_and_secret_free() {
    struct SuccessValidator;

    #[async_trait::async_trait]
    impl CredentialValidator for SuccessValidator {
        fn supports(&self, provider: &str) -> bool {
            provider == "anthropic"
        }

        async fn validate(
            &self,
            _provider: &str,
            _model: &str,
            _secret: &[u8],
            _endpoint: Option<&str>,
        ) -> Result<ValidatedIdentity, ValidationError> {
            Ok(ValidatedIdentity {
                identity: "validated-person@example.invalid".into(),
            })
        }
    }

    let command_id = "omitted-alias-command";
    let mut expected_hasher = blake3::Hasher::new();
    expected_hasher.update(b"haider-account-api-alias-v1\n");
    expected_hasher.update(command_id.as_bytes());
    let expected_digest = expected_hasher.finalize().to_hex();
    let expected_alias = format!("anthropic-api-{}", &expected_digest.as_str()[..12]);
    assert_eq!(canonical_api_alias("anthropic", command_id), expected_alias);
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(SuccessValidator),
        snapshot,
        management: None,
        profile_id: "omitted-alias".into(),
        default_model: "claude-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();
    commands
        .send(AccountCommand::Login(Box::new(LoginJob {
            command_id: command_id.into(),
            provider: "anthropic".into(),
            display_alias: None,
            validation_model: Some("claude-test".into()),
            secret: Some(Zeroizing::new(
                b"OMITTED_ALIAS_SECRET_SENTINEL_86b2".to_vec(),
            )),
            route: LoginRoute {
                request_id: RequestId::new("omitted-alias-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("send login");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("login deadline")
        .expect("login response")
    {
        WireFrame::Response {
            body: ResponseBody::AccountLoginApi { descriptor },
            ..
        } => {
            assert_eq!(descriptor.alias.as_str(), expected_alias);
            assert_eq!(descriptor.identity, "validated-person@example.invalid");
        }
        other => panic!("unexpected login response: {other:?}"),
    }
    assert!(
        vault
            .resolve(&CredentialAlias::new(expected_alias.clone()))
            .is_ok()
    );
    let receipts = store.login_receipts().await.expect("login receipts");
    let identity: LoginIdentity =
        serde_json::from_str(&receipts[0].request_json).expect("login identity");
    assert_eq!(
        identity.display_alias.as_deref(),
        Some(expected_alias.as_str())
    );
    assert_eq!(identity.physical_alias, expected_alias);
    assert!(!receipts[0].request_json.contains("SENTINEL"));
    assert!(!receipts[0].request_json.contains("validated-person"));

    commands
        .send(AccountCommand::Login(Box::new(LoginJob {
            command_id: command_id.into(),
            provider: "anthropic".into(),
            display_alias: None,
            validation_model: Some("claude-test".into()),
            secret: None,
            route: LoginRoute {
                request_id: RequestId::new("omitted-alias-replay"),
                sink,
            },
        })))
        .await
        .expect("send replay");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("replay deadline")
            .expect("replay response"),
        WireFrame::Response {
            body: ResponseBody::AccountLoginApi { .. },
            ..
        }
    ));
    actor.shutdown().await;
}

/// Provider configuration and default-model mutation share the account actor,
/// coherent snapshot, receipt replay, and revision CAS.
///
/// MUTATION CHECK: move `validate_default_model` above
/// `management_receipt_preflight` in `handle_set_default_model`. Expected
/// runtime failure: replaying `set-custom-default-b` after model B is removed
/// returns `invalid_argument` instead of its committed revision-two receipt.
#[tokio::test]
async fn provider_mutations_replay_before_validation_and_publish_one_snapshot() {
    struct CanonicalEndpointValidator {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ProviderEndpointValidator for CanonicalEndpointValidator {
        async fn validate(&self, origin: &str) -> Result<String, HaiderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(origin.trim_end_matches('/').to_owned())
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let accounts = memory_accounts();
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let management = ManagementSnapshot::new(0, Vec::new(), Vec::new());
    let endpoint_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: Arc::new(MemoryVault::new()),
        validator: Arc::new(ProviderCredentialValidator),
        snapshot,
        management: Some(management.clone()),
        profile_id: "provider-mutations".into(),
        default_model: "unused".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(CanonicalEndpointValidator {
            calls: Arc::clone(&endpoint_calls),
        }),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();

    let configure = |command_id: &str,
                     request_id: &str,
                     input: ProviderConfigureInput,
                     expected_revision: u64| {
        AccountCommand::ConfigureProvider(Box::new(ProviderConfigureJob {
            command_id: command_id.into(),
            input,
            expected_revision,
            route: LoginRoute {
                request_id: RequestId::new(request_id),
                sink: Arc::clone(&sink),
            },
        }))
    };
    commands
        .send(configure(
            "configure-custom",
            "configure-custom-request",
            ProviderConfigureInput {
                provider: "custom".into(),
                api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
                origin: Some("https://models.example.invalid/".into()),
                auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
                enabled: true,
                models: vec!["model-a".into(), "model-b".into()],
                default_model: Some("model-a".into()),
            },
            0,
        ))
        .await
        .expect("send configure");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("configure deadline")
        .expect("configure response")
    {
        WireFrame::Response {
            body: ResponseBody::ProviderConfigure { provider, revision },
            ..
        } => {
            assert_eq!(provider.provider, "custom");
            assert_eq!(
                provider.endpoint.as_deref(),
                Some("https://models.example.invalid")
            );
            assert_eq!(provider.default_model.as_deref(), Some("model-a"));
            assert_eq!(revision, 1);
        }
        other => panic!("unexpected configure response: {other:?}"),
    }
    assert_eq!(endpoint_calls.load(Ordering::SeqCst), 1);

    let set_default = |command_id: &str, request_id: &str, model: &str, expected_revision| {
        AccountCommand::SetDefaultModel(Box::new(SetDefaultModelJob {
            command_id: command_id.into(),
            provider: "custom".into(),
            model: model.into(),
            expected_revision,
            route: LoginRoute {
                request_id: RequestId::new(request_id),
                sink: Arc::clone(&sink),
            },
        }))
    };
    commands
        .send(set_default(
            "set-custom-default-b",
            "set-custom-default-b-request",
            "model-b",
            1,
        ))
        .await
        .expect("send default");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("default deadline")
            .expect("default response"),
        WireFrame::Response {
            body: ResponseBody::AccountSetDefaultModel { revision: 2, .. },
            ..
        }
    ));

    commands
        .send(configure(
            "configure-custom-models",
            "configure-custom-models-request",
            ProviderConfigureInput {
                provider: "custom".into(),
                api_family: None,
                origin: None,
                auth_requirement: None,
                enabled: true,
                models: vec!["model-a".into()],
                default_model: Some("model-a".into()),
            },
            2,
        ))
        .await
        .expect("send model update");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("model update deadline")
            .expect("model update response"),
        WireFrame::Response {
            body: ResponseBody::ProviderConfigure { revision: 3, .. },
            ..
        }
    ));
    assert_eq!(endpoint_calls.load(Ordering::SeqCst), 1);

    commands
        .send(set_default(
            "set-custom-default-b",
            "set-custom-default-b-replay",
            "model-b",
            1,
        ))
        .await
        .expect("send committed replay");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("replay deadline")
        .expect("replay response")
    {
        WireFrame::Response {
            body: ResponseBody::AccountSetDefaultModel { provider, revision },
            ..
        } => {
            assert_eq!(revision, 2);
            assert_eq!(provider.default_model.as_deref(), Some("model-b"));
        }
        other => panic!("unexpected default replay: {other:?}"),
    }

    commands
        .send(set_default(
            "stale-custom-default",
            "stale-custom-default-request",
            "model-b",
            1,
        ))
        .await
        .expect("send stale mutation");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("stale deadline")
        .expect("stale response")
    {
        WireFrame::Response {
            body:
                ResponseBody::Error {
                    code,
                    retryable,
                    data:
                        Some(ErrorData::RevisionConflict {
                            expected_revision,
                            current_revision,
                        }),
                    ..
                },
            ..
        } => {
            assert_eq!(code, ERROR_CODE_REVISION_CONFLICT);
            assert!(retryable);
            assert_eq!(expected_revision, 1);
            assert_eq!(current_revision, 3);
        }
        other => panic!("unexpected stale response: {other:?}"),
    }

    let view = management.read().expect("management snapshot");
    assert_eq!(view.revision, 3);
    let custom = view
        .providers
        .iter()
        .find(|provider| provider.provider == "custom")
        .expect("custom provider");
    assert!(
        custom.models.iter().any(|model| model == "model-a")
            && custom.models.iter().any(|model| model == "model-b"),
        "management keeps the injected discovered inventory, not provider.configure literals"
    );
    assert_eq!(custom.default_model.as_deref(), Some("model-a"));
    drop(view);
    actor.shutdown().await;
}

/// Startup reconciliation keeps a failed vault deletion pending and reserved,
/// then completes it idempotently on the next reconciliation attempt.
///
/// MUTATION CHECK: remove the `continue` from the failed-delete branch in
/// `reconcile_remove_receipts`, allowing receipt finalization after a failed
/// vault delete. Expected runtime failure: the first-phase reservation and
/// pending-receipt assertions below observe an empty/committed state while
/// the orphan secret is still present.
#[tokio::test]
async fn pending_remove_reconciliation_retries_orphan_deletion_before_release() {
    struct DeleteGateVault {
        inner: MemoryVault,
        fail_delete: AtomicBool,
    }

    impl Vault for DeleteGateVault {
        fn put(
            &self,
            alias: &CredentialAlias,
            secret: &[u8],
        ) -> haider_accounts::AccountsResult<()> {
            self.inner.put(alias, secret)
        }

        fn resolve(
            &self,
            alias: &CredentialAlias,
        ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
            self.inner.resolve(alias)
        }

        fn delete(&self, alias: &CredentialAlias) -> haider_accounts::AccountsResult<()> {
            if self.fail_delete.load(Ordering::SeqCst) {
                return Err(HaiderError::new(
                    ErrorCode::Internal,
                    "injected vault deletion failure",
                    true,
                ));
            }
            self.inner.delete(alias)
        }

        fn list(&self) -> haider_accounts::AccountsResult<Vec<CredentialAlias>> {
            self.inner.list()
        }
    }

    let alias = CredentialAlias::new("remove-orphan");
    let mut accounts = memory_accounts();
    accounts
        .add(CredentialDescriptor {
            alias: alias.clone(),
            provider: "anthropic".into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "orphan fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        })
        .expect("descriptor");
    let vault = Arc::new(DeleteGateVault {
        inner: MemoryVault::new(),
        fail_delete: AtomicBool::new(true),
    });
    vault
        .put(&alias, b"ORPHAN_REMOVE_SECRET_441a")
        .expect("seed secret");
    let provision = VaultProvision::Available(vault.clone() as Arc<dyn Vault>);
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let request_json = r#"{"alias":"remove-orphan","expected_revision":0}"#.to_owned();
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    assert!(matches!(
        store
            .account_remove_claim_receipt::<RemoveReceipt>(
                "pending-remove-orphan".into(),
                request_digest,
                request_json,
                r#"{"provider":"anthropic","was_active":true}"#.into(),
                Some(0),
                alias.as_str().into(),
                "anthropic".into(),
                true,
            )
            .await
            .expect("claim remove"),
        ManagementClaim::Fresh
    ));

    reconcile_remove_receipts(&store, &mut accounts, &provision)
        .await
        .expect("first reconciliation");
    assert!(accounts.get(&alias).is_none());
    assert!(vault.resolve(&alias).is_ok(), "failed delete leaves orphan");
    assert_eq!(
        store
            .reserved_account_aliases()
            .await
            .expect("reserved aliases"),
        vec![alias.as_str().to_owned()]
    );
    assert_eq!(
        store
            .account_remove_receipts()
            .await
            .expect("pending receipt")
            .len(),
        1
    );
    assert_eq!(store.management_revision().await.expect("revision"), 0);

    vault.fail_delete.store(false, Ordering::SeqCst);
    reconcile_remove_receipts(&store, &mut accounts, &provision)
        .await
        .expect("second reconciliation");
    assert!(vault.resolve(&alias).is_err());
    assert!(
        store
            .reserved_account_aliases()
            .await
            .expect("released reservation")
            .is_empty()
    );
    assert!(
        store
            .account_remove_receipts()
            .await
            .expect("finalized receipt")
            .is_empty()
    );
    assert_eq!(store.management_revision().await.expect("revision"), 1);
}

/// Pending W5c provider receipts remain recoverable immediately after the
/// v8 migration, before any discovered catalog exists.
///
/// MUTATION CHECK: replace the `reconcile_set_default_model` and
/// `reconcile_configure` calls in `reconcile_provider_receipts` with their
/// normal discovered-only twins. Expected runtime failure:
/// `reconcile_provider_receipts` returns `InvalidArgument` because the
/// migrated cache is empty, leaving both receipts pending.
/// Verified by revert on 2026-07-30.
#[tokio::test]
async fn pre_v8_pending_provider_receipts_reconcile_without_a_discovered_cache() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let legacy_profiles = vec![
        ProviderProfileV1 {
            provider_id: "legacy-default".to_owned(),
            display_name: "legacy-default".to_owned(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some("https://legacy-default.example.invalid".to_owned()),
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: vec![
                "frontier-legacy-a".to_owned(),
                "frontier-legacy-b".to_owned(),
            ],
            default_model: Some("frontier-legacy-a".to_owned()),
            provenance: crate::provider_registry::ProviderProvenance::Custom,
        },
        ProviderProfileV1 {
            provider_id: "legacy-configure".to_owned(),
            display_name: "legacy-configure".to_owned(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some("https://legacy-configure.example.invalid".to_owned()),
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: vec!["frontier-legacy-old".to_owned()],
            default_model: Some("frontier-legacy-old".to_owned()),
            provenance: crate::provider_registry::ProviderProvenance::Custom,
        },
    ];
    let provider_store: Box<dyn ProviderRegistryStoreLike> =
        Box::new(TestProviderStore(StdMutex::new(legacy_profiles.clone())));
    let mut providers = ProviderRegistry::new(
        provider_store,
        legacy_profiles,
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("legacy provider registry");

    let default_identity = SetDefaultModelIdentity {
        provider: "legacy-default".to_owned(),
        model: "frontier-legacy-b".to_owned(),
        expected_revision: 0,
    };
    let (default_json, default_digest) = command_json(&default_identity).expect("default identity");
    assert!(matches!(
        store
            .management_claim_receipt::<ProviderReceipt>(
                "legacy-default-pending".to_owned(),
                ACCOUNT_SET_DEFAULT_MODEL_METHOD.to_owned(),
                default_digest,
                default_json,
                None,
                Some(0),
            )
            .await
            .expect("claim legacy default"),
        ManagementClaim::Fresh
    ));

    let configure_input = ProviderConfigureInput {
        provider: "legacy-configure".to_owned(),
        api_family: None,
        origin: None,
        auth_requirement: None,
        enabled: true,
        models: vec!["frontier-legacy-new".to_owned()],
        default_model: Some("frontier-legacy-new".to_owned()),
    };
    let configure_identity = ProviderConfigureIdentity {
        input: configure_input.clone(),
        expected_revision: 0,
    };
    let (configure_json, configure_digest) =
        command_json(&configure_identity).expect("configure identity");
    assert!(matches!(
        store
            .management_claim_receipt::<ProviderReceipt>(
                "legacy-configure-pending".to_owned(),
                PROVIDER_CONFIGURE_METHOD.to_owned(),
                configure_digest,
                configure_json,
                Some(serde_json::to_string(&configure_input).expect("recovery JSON")),
                Some(0),
            )
            .await
            .expect("claim legacy configure"),
        ManagementClaim::Fresh
    ));

    reconcile_provider_receipts(&store, &memory_accounts(), &mut providers)
        .await
        .expect("legacy receipt reconciliation");
    assert_eq!(store.management_revision().await.expect("revision"), 2);
    assert!(
        store
            .management_receipts(ACCOUNT_SET_DEFAULT_MODEL_METHOD.to_owned())
            .await
            .expect("default receipts")
            .iter()
            .all(|row| row.state == "committed")
    );
    assert!(
        store
            .management_receipts(PROVIDER_CONFIGURE_METHOD.to_owned())
            .await
            .expect("configure receipts")
            .iter()
            .all(|row| row.state == "committed")
    );
    let configured = providers
        .get("legacy-configure")
        .expect("reconciled configured profile");
    assert_eq!(configured.configured_models, vec!["frontier-legacy-new"]);
    let summary = providers
        .summary("legacy-configure", &|_| false)
        .expect("legacy summary");
    assert!(summary.models.is_empty());
    assert_eq!(summary.default_model, None);
    store.close().await.expect("close");
}

struct UnusedIdentityVerifier;

#[async_trait::async_trait]
impl crate::oauth::OAuthIdentityVerifier for UnusedIdentityVerifier {
    async fn verify(
        &self,
        _id_token: &[u8],
        _expected: crate::oauth::OAuthIdentityExpectation<'_>,
    ) -> Result<haider_accounts::OAuthIdentityV1, crate::oauth::OAuthPublicError> {
        panic!("the unexpired model-refresh fixture must not verify an ID token")
    }
}

type ModelDiscoveryObservation = (CatalogSource, Option<String>, Option<String>);

struct BlockingModelDiscoverer {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    seen: StdMutex<Vec<ModelDiscoveryObservation>>,
    results: StdMutex<std::collections::VecDeque<ModelDiscoveryFixture>>,
}

enum ModelDiscoveryFixture {
    Return(Result<DiscoveredCatalog, CatalogError>),
    Panic,
}

/// WH3 registry/refresh projection half — the named source resolves to the
/// fixed DeepSeek catalog and an authenticated mocked discovery replaces
/// the fallback inventory while lighting the pair Available.
#[tokio::test]
async fn wh3_deepseek_catalog_source_populates_models_and_flips_available() {
    let provider_store: Box<dyn ProviderRegistryStoreLike> = Box::new(TestProviderStore::default());
    let providers = ProviderRegistry::new(
        provider_store,
        initial_provider_profiles(
            &std::collections::BTreeSet::from([DEEPSEEK_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("DeepSeek provider registry");
    let (source, auth) =
        catalog_source(DEEPSEEK_PROVIDER_NAME, &providers).expect("DeepSeek catalog source");
    assert_eq!(source, CatalogSource::DeepSeekApi);
    assert_eq!(source.endpoint(), "https://api.deepseek.com/models");
    assert_eq!(auth, ProviderAuthRequirementWire::ApiKey);

    let before = providers
        .summary(DEEPSEEK_PROVIDER_NAME, &|_| false)
        .expect("signed-out DeepSeek");
    assert_eq!(
        before.availability,
        haider_rpc::ProviderAvailabilityWire::Unavailable
    );

    let discoverer = Arc::new(BlockingModelDiscoverer {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        seen: StdMutex::new(Vec::new()),
        results: StdMutex::new(std::collections::VecDeque::from([
            ModelDiscoveryFixture::Return(Ok(DiscoveredCatalog {
                models: vec![haider_provider::DiscoveredModel {
                    slug: "deepseek-live-fixture".to_owned(),
                    display_name: "DeepSeek Live Fixture".to_owned(),
                    context_window: None,
                    description: None,
                    default_effort: None,
                    supported_efforts: Vec::new(),
                    visible: true,
                    priority: None,
                    extensions: None,
                }],
                etag: None,
            })),
        ])),
    });
    let discovery = {
        let discoverer = Arc::clone(&discoverer);
        let source = source.clone();
        tokio::spawn(async move {
            discoverer
                .discover(source, Some("DEEPSEEK_DISCOVERY_KEY_SENTINEL_43af"), None)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("mock DeepSeek GET started");
    discoverer.release.notify_one();
    let discovered = discovery
        .await
        .expect("mock DeepSeek discovery task")
        .expect("mock DeepSeek /models response");
    assert_eq!(
        discoverer.seen.lock().expect("seen").as_slice(),
        [(
            CatalogSource::DeepSeekApi,
            Some("DEEPSEEK_DISCOVERY_KEY_SENTINEL_43af".to_owned()),
            None,
        )]
    );
    providers.replace_models(DEEPSEEK_PROVIDER_NAME.to_owned(), discovered.models);
    let after = providers
        .summary(DEEPSEEK_PROVIDER_NAME, &|provider| {
            provider == DEEPSEEK_PROVIDER_NAME
        })
        .expect("discovered DeepSeek");
    assert_eq!(after.models, ["deepseek-live-fixture"]);
    assert_eq!(
        after.availability,
        haider_rpc::ProviderAvailabilityWire::Available
    );
    assert!(after.model_details[0].supported_efforts.is_empty());
    assert!(after.model_details[0].supported_speeds.is_empty());
}

#[async_trait::async_trait]
impl ProviderModelDiscoverer for BlockingModelDiscoverer {
    async fn discover(
        &self,
        source: CatalogSource,
        access_token: Option<&str>,
        etag: Option<&str>,
    ) -> Result<DiscoveredCatalog, CatalogError> {
        self.seen.lock().expect("seen lock").push((
            source,
            access_token.map(str::to_owned),
            etag.map(str::to_owned),
        ));
        self.started.notify_one();
        self.release.notified().await;
        let result = self
            .results
            .lock()
            .expect("results lock")
            .pop_front()
            .expect("queued discovery result");
        match result {
            ModelDiscoveryFixture::Return(result) => result,
            ModelDiscoveryFixture::Panic => panic!("injected model discovery panic"),
        }
    }
}

fn refresh_oauth_catalog() -> OAuthProviderCatalog {
    let registration = crate::oauth::OAuthProviderRegistration::new(
        OPENAI_OAUTH_PROVIDER_NAME,
        "http://127.0.0.1:32111",
        "http://127.0.0.1:32111/authorize",
        "http://127.0.0.1:32111/token",
        "model-refresh-client",
        vec!["inference".to_owned()],
        "model-refresh-audience",
        Some("model-refresh-resource".to_owned()),
        true,
        Arc::new(UnusedIdentityVerifier),
    )
    .expect("test registration");
    OAuthProviderCatalog::with_test_registrations([registration]).expect("test catalog")
}

fn refresh_oauth_bundle() -> haider_accounts::OAuthTokenBundleV1 {
    haider_accounts::OAuthTokenBundleV1::new(
        OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        "http://127.0.0.1:32111".to_owned(),
        "model-refresh-audience".to_owned(),
        Some("model-refresh-resource".to_owned()),
        "Bearer".to_owned(),
        Zeroizing::new(b"MODEL_REFRESH_ACCESS_SENTINEL_73c1".to_vec()),
        Some(Zeroizing::new(
            b"MODEL_REFRESH_UNUSED_REFRESH_SENTINEL_442a".to_vec(),
        )),
        u64::MAX - 1,
        Some(u64::MAX),
        vec!["inference".to_owned()],
        haider_accounts::OAuthIdentityV1 {
            subject_hash: "model-refresh-subject".to_owned(),
            display_identity: "model refresh fixture".to_owned(),
        },
        1,
    )
    .expect("model refresh bundle")
}

/// MUTATION CHECK (W5g-5b): drop the custom-profile arm from
/// `catalog_source`. Expected runtime failure: the first refresh answers
/// unavailable instead of publishing `custom-key-model` in its summary.
///
/// MUTATION CHECK: source custom discovery from the credential descriptor.
/// Expected runtime failure: the fake observes the descriptor's decoy origin
/// instead of the registry profile's stored origin.
///
/// MUTATION CHECK: always attach a bearer credential. Expected runtime
/// failure: the no-auth profile's second discovery observation contains a
/// token instead of `None`.
#[tokio::test]
async fn custom_provider_refresh_uses_stored_origin_and_publishes_discovered_slugs() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let key_provider = "custom-refresh-key";
    let no_auth_provider = "custom-refresh-none";
    let key_origin = "http://127.0.0.1:12401/v1";
    let no_auth_origin = "https://no-auth-models.example.invalid/v1";
    let profiles = vec![
        ProviderProfileV1 {
            provider_id: key_provider.to_owned(),
            display_name: key_provider.to_owned(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some(key_origin.to_owned()),
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: vec!["seed-key".to_owned()],
            default_model: Some("seed-key".to_owned()),
            provenance: ProviderProvenance::Custom,
        },
        ProviderProfileV1 {
            provider_id: no_auth_provider.to_owned(),
            display_name: no_auth_provider.to_owned(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some(no_auth_origin.to_owned()),
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::None,
            configured_models: vec!["seed-none".to_owned()],
            default_model: Some("seed-none".to_owned()),
            provenance: ProviderProvenance::Custom,
        },
        ProviderProfileV1 {
            provider_id: GEMINI_PROVIDER_NAME.to_owned(),
            display_name: GEMINI_PROVIDER_NAME.to_owned(),
            api_family: ProviderApiFamilyWire::GeminiGenerateContent,
            base_url: Some(haider_provider::GEMINI_API_BASE_URL.to_owned()),
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: Vec::new(),
            default_model: None,
            provenance: ProviderProvenance::BuiltIn,
        },
    ];
    let provider_store: Box<dyn ProviderRegistryStoreLike> =
        Box::new(TestProviderStore(StdMutex::new(profiles)));
    let providers = ProviderRegistry::new(
        provider_store,
        Vec::new(),
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("custom provider registry");

    let alias = CredentialAlias::new("custom-refresh-key");
    let descriptor = CredentialDescriptor {
        alias: alias.clone(),
        provider: key_provider.to_owned(),
        base_url: Some("http://203.0.113.7/descriptor-decoy".to_owned()),
        auth_method: AuthMethod::ApiKey,
        identity: "custom refresh key".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let gemini_alias = CredentialAlias::new("gemini-refresh-key");
    let gemini_descriptor = CredentialDescriptor {
        alias: gemini_alias.clone(),
        provider: GEMINI_PROVIDER_NAME.to_owned(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "Gemini refresh key".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let mut accounts = memory_accounts();
    accounts.add(descriptor).expect("custom descriptor");
    accounts.add(gemini_descriptor).expect("Gemini descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(&alias, b"CUSTOM_REFRESH_API_KEY_SENTINEL_5b")
        .expect("seed custom API key");
    vault
        .put(&gemini_alias, b"GEMINI_REFRESH_API_KEY_SENTINEL_8c")
        .expect("seed Gemini API key");
    let management =
        ManagementSnapshot::new(0, accounts.list().to_vec(), providers.summaries(&|_| false));
    let discoverer = Arc::new(BlockingModelDiscoverer {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        seen: StdMutex::new(Vec::new()),
        results: StdMutex::new(std::collections::VecDeque::from([
            ModelDiscoveryFixture::Return(Ok(DiscoveredCatalog {
                models: vec![haider_provider::DiscoveredModel {
                    slug: "custom-key-model".to_owned(),
                    display_name: "custom-key-model".to_owned(),
                    context_window: None,
                    description: None,
                    default_effort: None,
                    supported_efforts: Vec::new(),
                    visible: true,
                    priority: None,
                    extensions: None,
                }],
                etag: None,
            })),
            ModelDiscoveryFixture::Return(Ok(DiscoveredCatalog {
                models: vec![haider_provider::DiscoveredModel {
                    slug: "gemini-2.5-flash".to_owned(),
                    display_name: "Gemini 2.5 Flash".to_owned(),
                    context_window: Some(1_048_576),
                    description: None,
                    default_effort: None,
                    supported_efforts: Vec::new(),
                    visible: true,
                    priority: None,
                    extensions: None,
                }],
                etag: Some(r#"W/"gemini-catalog""#.to_owned()),
            })),
            ModelDiscoveryFixture::Return(Ok(DiscoveredCatalog {
                models: vec![haider_provider::DiscoveredModel {
                    slug: "custom-public-model".to_owned(),
                    display_name: "custom-public-model".to_owned(),
                    context_window: None,
                    description: None,
                    default_effort: None,
                    supported_efforts: Vec::new(),
                    visible: true,
                    priority: None,
                    extensions: None,
                }],
                etag: None,
            })),
        ])),
    });
    let discoverer_trait: Arc<dyn ProviderModelDiscoverer> = discoverer.clone();
    let broker_vault = vault.clone() as Arc<dyn Vault>;
    let broker_snapshot = Arc::clone(&snapshot);
    let (mut actor, _broker) = start_account_actor_with_services(
        AccountActorConfig {
            store: store.clone(),
            accounts,
            vault: vault as Arc<dyn Vault>,
            validator: Arc::new(ProviderCredentialValidator),
            snapshot: Arc::clone(&snapshot),
            management: Some(management),
            profile_id: "custom-refresh".to_owned(),
            default_model: "unused".to_owned(),
            providers,
            provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
            reserved_aliases: HashSet::new(),
            refresh_fences: RefreshFenceRegistry::default(),
        },
        |commands| {
            CredentialBroker::new(
                broker_vault,
                OAuthProviderCatalog::default(),
                broker_snapshot,
                commands,
            )
        },
        discoverer_trait,
        Arc::new(UnreachableGcloud),
        Arc::new(PlatformClaudeNativeCredentialStore::default()),
    )
    .expect("custom refresh actor");

    let (key_sink, mut key_frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::RefreshProviderModels {
            provider: key_provider.to_owned(),
            completed: LoginRoute {
                request_id: RequestId::new("custom-key-refresh"),
                sink: key_sink,
            },
        })
        .await
        .expect("key refresh handoff");
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("key discovery started");
    discoverer.release.notify_one();
    let key_frame = tokio::time::timeout(Duration::from_secs(2), key_frames.recv())
        .await
        .expect("key refresh deadline")
        .expect("key refresh response");
    match key_frame {
        WireFrame::Response {
            body: ResponseBody::ProviderModelsRefresh { provider, .. },
            ..
        } => assert_eq!(provider.models, vec!["custom-key-model"]),
        other => panic!("unexpected key refresh response: {other:?}"),
    }

    let (gemini_sink, mut gemini_frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::RefreshProviderModels {
            provider: GEMINI_PROVIDER_NAME.to_owned(),
            completed: LoginRoute {
                request_id: RequestId::new("gemini-key-refresh"),
                sink: gemini_sink,
            },
        })
        .await
        .expect("Gemini refresh handoff");
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("Gemini discovery started");
    discoverer.release.notify_one();
    let gemini_frame = tokio::time::timeout(Duration::from_secs(2), gemini_frames.recv())
        .await
        .expect("Gemini refresh deadline")
        .expect("Gemini refresh response");
    match gemini_frame {
        WireFrame::Response {
            body: ResponseBody::ProviderModelsRefresh { provider, .. },
            ..
        } => {
            assert_eq!(provider.models, vec!["gemini-2.5-flash"]);
            assert_eq!(provider.model_details[0].context_window, Some(1_048_576));
        }
        other => panic!("unexpected Gemini refresh response: {other:?}"),
    }

    let (none_sink, mut none_frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::RefreshProviderModels {
            provider: no_auth_provider.to_owned(),
            completed: LoginRoute {
                request_id: RequestId::new("custom-none-refresh"),
                sink: none_sink,
            },
        })
        .await
        .expect("no-auth refresh handoff");
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("no-auth discovery started");
    discoverer.release.notify_one();
    let none_frame = tokio::time::timeout(Duration::from_secs(2), none_frames.recv())
        .await
        .expect("no-auth refresh deadline")
        .expect("no-auth refresh response");
    match none_frame {
        WireFrame::Response {
            body: ResponseBody::ProviderModelsRefresh { provider, .. },
            ..
        } => assert_eq!(provider.models, vec!["custom-public-model"]),
        other => panic!("unexpected no-auth refresh response: {other:?}"),
    }

    assert_eq!(
        discoverer.seen.lock().expect("seen").as_slice(),
        [
            (
                CatalogSource::OpenAiCompatible {
                    origin: key_origin.to_owned()
                },
                Some("CUSTOM_REFRESH_API_KEY_SENTINEL_5b".to_owned()),
                None,
            ),
            (
                CatalogSource::GeminiApiKey,
                Some("GEMINI_REFRESH_API_KEY_SENTINEL_8c".to_owned()),
                None,
            ),
            (
                CatalogSource::OpenAiCompatible {
                    origin: no_auth_origin.to_owned()
                },
                None,
                None,
            ),
        ],
        "discovery uses only stored profile origins and the declared auth mode"
    );
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// Refresh HTTP is handed to an owned task, broker resolution exposes only
/// the OAuth access token, and the actor alone publishes the durable result.
///
/// MUTATION CHECK 1: after `begin_provider_models_refresh` returns in the
/// `RefreshProviderModels` actor arm, await `model_refreshes.join_next()`
/// before receiving another command. Expected runtime failure: the
/// `ResolveCredential` request times out while discovery is blocked.
///
/// MUTATION CHECK 2: replace `management.publish(...)` in the successful
/// refresh completion with `management.publish_accounts(...)`. Expected
/// runtime failure: the final management snapshot still contains the old
/// fixture inventory instead of `frontier-refresh`.
///
/// MUTATION CHECK 3: replace the cached ETag extraction in
/// `begin_provider_models_refresh` with `let etag = None`. Expected runtime
/// failure: the second discovery observation has no `W/"refresh-etag"`
/// conditional validator.
///
/// MUTATION CHECK 4: in the `CatalogError::Unavailable` completion arm,
/// upsert the prior cache with a new timestamp before responding. Expected
/// runtime failure: the exact cached-row equality after the third refresh
/// observes a changed `fetched_at_ms`.
///
/// MUTATION CHECK 5: drop the JoinError cleanup/response in the actor's
/// `model_refreshes.join_next_with_id()` arm. Expected runtime failure: the
/// panicked discovery's correlation receives no retryable provider error
/// (and its retry remains permanently busy).
///
/// MUTATION CHECK 6: restore the immediate `break` in the
/// `AccountCommand::Shutdown` arm. Expected runtime failure: graceful
/// shutdown completes while discovery is still blocked and drops the
/// accepted refresh response.
/// All six verified by revert on 2026-07-30.
#[tokio::test]
async fn provider_model_refresh_does_not_block_actor_and_publishes_cache_provenance() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let alias = CredentialAlias::new("model-refresh-active");
    let descriptor = CredentialDescriptor {
        alias: alias.clone(),
        provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "model refresh fixture".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let mut accounts = memory_accounts();
    accounts.add(descriptor.clone()).expect("descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(
            &alias,
            &refresh_oauth_bundle().encode().expect("encode bundle"),
        )
        .expect("seed OAuth bundle");
    let providers = test_provider_registry();
    let management =
        ManagementSnapshot::new(0, accounts.list().to_vec(), providers.summaries(&|_| false));
    let discoverer = Arc::new(BlockingModelDiscoverer {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        seen: StdMutex::new(Vec::new()),
        results: StdMutex::new(std::collections::VecDeque::from([
            ModelDiscoveryFixture::Return(Ok(DiscoveredCatalog {
                models: vec![haider_provider::DiscoveredModel {
                    slug: "frontier-refresh".to_owned(),
                    display_name: "Provider Refresh Fixture".to_owned(),
                    context_window: None,
                    description: Some("provider-owned refresh provenance".to_owned()),
                    default_effort: Some("medium".to_owned()),
                    supported_efforts: vec!["low".to_owned(), "medium".to_owned()],
                    visible: true,
                    priority: Some(7),
                    extensions: None,
                }],
                etag: Some(r#"W/"refresh-etag""#.to_owned()),
            })),
            ModelDiscoveryFixture::Return(Err(CatalogError::NotModified)),
            ModelDiscoveryFixture::Return(Err(CatalogError::Unavailable {
                reason: "fixture catalog is unavailable".to_owned(),
            })),
            ModelDiscoveryFixture::Panic,
            ModelDiscoveryFixture::Return(Err(CatalogError::NotModified)),
            ModelDiscoveryFixture::Return(Err(CatalogError::NotModified)),
        ])),
    });
    let discoverer_trait: Arc<dyn ProviderModelDiscoverer> = discoverer.clone();
    let broker_vault = vault.clone() as Arc<dyn Vault>;
    let broker_snapshot = Arc::clone(&snapshot);
    let (mut actor, broker) = start_account_actor_with_services(
        AccountActorConfig {
            store: store.clone(),
            accounts,
            vault: vault as Arc<dyn Vault>,
            validator: Arc::new(ProviderCredentialValidator),
            snapshot: Arc::clone(&snapshot),
            management: Some(management.clone()),
            profile_id: "model-refresh".to_owned(),
            default_model: "unused".to_owned(),
            providers,
            provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
            reserved_aliases: HashSet::new(),
            refresh_fences: RefreshFenceRegistry::default(),
        },
        |commands| {
            CredentialBroker::new(
                broker_vault,
                refresh_oauth_catalog(),
                broker_snapshot,
                commands,
            )
        },
        discoverer_trait,
        Arc::new(UnreachableGcloud),
        Arc::new(PlatformClaudeNativeCredentialStore::default()),
    )
    .expect("actor with broker");

    let (sink, mut frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::RefreshProviderModels {
            provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
            completed: LoginRoute {
                request_id: RequestId::new("refresh-models"),
                sink,
            },
        })
        .await
        .expect("refresh handoff");
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("discovery started");

    let (completed, resolved) = tokio::sync::oneshot::channel();
    actor
        .commands()
        .send(AccountCommand::ResolveCredential {
            provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
            failure: None,
            completed,
        })
        .await
        .expect("resolver handoff");
    let resolved = tokio::time::timeout(Duration::from_secs(1), resolved)
        .await
        .expect("actor remained responsive")
        .expect("resolver response")
        .expect("active descriptor");
    assert_eq!(resolved.descriptor.alias, alias);

    discoverer.release.notify_one();
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("refresh response deadline")
        .expect("refresh response");
    match frame {
        WireFrame::Response {
            request_id,
            body: ResponseBody::ProviderModelsRefresh { provider, revision },
        } => {
            assert_eq!(request_id.as_str(), "refresh-models");
            assert_eq!(revision, 1);
            assert_eq!(provider.models, vec!["frontier-refresh"]);
            assert_eq!(provider.default_model, None);
        }
        other => panic!("unexpected refresh response: {other:?}"),
    }

    let seen = discoverer.seen.lock().expect("seen lock").clone();
    assert_eq!(
        seen.first(),
        Some(&(
            CatalogSource::OpenAiSubscription,
            Some("MODEL_REFRESH_ACCESS_SENTINEL_73c1".to_owned()),
            None,
        )),
        "discovery receives the broker-extracted access token, never the encoded bundle"
    );
    let cached = store
        .provider_models(OPENAI_OAUTH_PROVIDER_NAME.to_owned())
        .await
        .expect("cache read")
        .expect("cache row");
    let cached_models: Vec<haider_provider::DiscoveredModel> =
        serde_json::from_str(&cached.models_json).expect("cached catalog");
    assert_eq!(cached_models[0].slug, "frontier-refresh");
    assert_eq!(cached.etag.as_deref(), Some(r#"W/"refresh-etag""#));
    let view = management.read().expect("management view");
    assert_eq!(view.revision, 1);
    let summary = view
        .providers
        .iter()
        .find(|summary| summary.provider == OPENAI_OAUTH_PROVIDER_NAME)
        .expect("refreshed provider");
    assert_eq!(summary.models, vec!["frontier-refresh"]);

    tokio::time::sleep(Duration::from_millis(2)).await;
    let (sink, mut not_modified_frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::RefreshProviderModels {
            provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
            completed: LoginRoute {
                request_id: RequestId::new("refresh-models-not-modified"),
                sink,
            },
        })
        .await
        .expect("conditional refresh handoff");
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("conditional discovery started");
    discoverer.release.notify_one();
    let frame = tokio::time::timeout(Duration::from_secs(2), not_modified_frames.recv())
        .await
        .expect("not-modified response deadline")
        .expect("not-modified response");
    assert!(matches!(
        frame,
        WireFrame::Response {
            body: ResponseBody::ProviderModelsRefresh { revision: 1, .. },
            ..
        }
    ));
    let touched = store
        .provider_models(OPENAI_OAUTH_PROVIDER_NAME.to_owned())
        .await
        .expect("touched cache read")
        .expect("touched cache row");
    assert_eq!(touched.models_json, cached.models_json);
    assert_eq!(touched.etag, cached.etag);
    assert!(
        touched.fetched_at_ms > cached.fetched_at_ms,
        "304 refresh must touch only the fetch timestamp"
    );
    assert_eq!(store.management_revision().await.expect("revision"), 1);
    let seen = discoverer.seen.lock().expect("seen lock").clone();
    assert_eq!(
        seen.get(1).and_then(|(_, _, etag)| etag.as_deref()),
        Some(r#"W/"refresh-etag""#)
    );

    tokio::time::sleep(Duration::from_millis(2)).await;
    let before_unavailable = touched;
    let (sink, mut unavailable_frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::RefreshProviderModels {
            provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
            completed: LoginRoute {
                request_id: RequestId::new("refresh-models-unavailable"),
                sink,
            },
        })
        .await
        .expect("unavailable refresh handoff");
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("unavailable discovery started");
    discoverer.release.notify_one();
    let frame = tokio::time::timeout(Duration::from_secs(2), unavailable_frames.recv())
        .await
        .expect("unavailable response deadline")
        .expect("unavailable response");
    match frame {
        WireFrame::Response {
            body:
                ResponseBody::Error {
                    code,
                    message,
                    retryable,
                    data: Some(ErrorData::ProviderModelsUnavailable { provider, reason }),
                },
            ..
        } => {
            assert_eq!(code, ERROR_CODE_PROVIDER_ERROR);
            assert_eq!(message, "fixture catalog is unavailable");
            assert!(!retryable);
            assert_eq!(provider, OPENAI_OAUTH_PROVIDER_NAME);
            assert_eq!(reason, "fixture catalog is unavailable");
        }
        other => panic!("unexpected unavailable response: {other:?}"),
    }
    assert_eq!(
        store
            .provider_models(OPENAI_OAUTH_PROVIDER_NAME.to_owned())
            .await
            .expect("cache after unavailable")
            .expect("cache remains"),
        before_unavailable
    );
    assert_eq!(store.management_revision().await.expect("revision"), 1);

    let (sink, mut panic_frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::RefreshProviderModels {
            provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
            completed: LoginRoute {
                request_id: RequestId::new("refresh-models-panic"),
                sink,
            },
        })
        .await
        .expect("panic refresh handoff");
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("panic discovery started");
    discoverer.release.notify_one();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), panic_frames.recv())
            .await
            .expect("panic response deadline")
            .expect("panic response"),
        WireFrame::Response {
            body:
                ResponseBody::Error {
                    code,
                    retryable: true,
                    data: None,
                    ..
                },
            ..
        } if code == ERROR_CODE_PROVIDER_ERROR
    ));

    let (sink, mut recovered_frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::RefreshProviderModels {
            provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
            completed: LoginRoute {
                request_id: RequestId::new("refresh-models-after-panic"),
                sink,
            },
        })
        .await
        .expect("post-panic refresh handoff");
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("post-panic discovery started");
    discoverer.release.notify_one();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), recovered_frames.recv())
            .await
            .expect("post-panic response deadline")
            .expect("post-panic response"),
        WireFrame::Response {
            body: ResponseBody::ProviderModelsRefresh { revision: 1, .. },
            ..
        }
    ));

    let (sink, mut draining_frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::RefreshProviderModels {
            provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
            completed: LoginRoute {
                request_id: RequestId::new("refresh-models-during-drain"),
                sink,
            },
        })
        .await
        .expect("draining refresh handoff");
    tokio::time::timeout(Duration::from_secs(2), discoverer.started.notified())
        .await
        .expect("draining discovery started");
    let mut shutdown = tokio::spawn(async move {
        actor.shutdown().await;
        actor
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut shutdown)
            .await
            .is_err(),
        "graceful shutdown must wait for the accepted discovery"
    );
    discoverer.release.notify_one();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), draining_frames.recv())
            .await
            .expect("draining response deadline")
            .expect("draining response"),
        WireFrame::Response {
            body: ResponseBody::ProviderModelsRefresh { revision: 1, .. },
            ..
        }
    ));
    let _actor = tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("graceful actor shutdown deadline")
        .expect("graceful actor shutdown");
    assert!(broker.shutdown().await);
    store.close().await.expect("close");
}

/// Release evidence, never the merge gate (§6.2): the PACKAGED default
/// model ID must validate a real key through the production
/// `AnthropicValidator` (the same audited one-token Messages path `/login`
/// uses). Run manually: `HAIDER_ANTHROPIC_API_KEY=... cargo test -p
/// haider-daemon --lib live_smoke -- --ignored`.
#[tokio::test]
#[ignore = "live Anthropic API smoke; requires HAIDER_ANTHROPIC_API_KEY"]
async fn live_smoke_packaged_default_model_validates_a_real_key() {
    let key = std::env::var("HAIDER_ANTHROPIC_API_KEY")
        .unwrap_or_else(|_| panic!("HAIDER_ANTHROPIC_API_KEY must be set for the live smoke"));
    let identity = AnthropicValidator
        .validate(
            "anthropic",
            haider_client::PACKAGED_DEFAULT_MODEL,
            key.trim().as_bytes(),
            None,
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "packaged default model {} failed live validation: {:?} {}",
                haider_client::PACKAGED_DEFAULT_MODEL,
                error.kind,
                error.message
            )
        });
    assert!(!identity.identity.is_empty());
}

async fn send_oauth_import(
    commands: &mpsc::Sender<AccountCommand>,
    sink: Arc<dyn FrameSink>,
    command_id: &str,
    source: &str,
) {
    commands
        .send(AccountCommand::ImportOAuth(Box::new(OAuthImportJob {
            command_id: command_id.to_owned(),
            source: source.to_owned(),
            route: LoginRoute {
                request_id: RequestId::new(format!("{command_id}-request")),
                sink,
            },
        })))
        .await
        .expect("send OAuth import");
}

async fn import_codex_for_heal(actor: &AccountActorHandle) -> CredentialDescriptor {
    let (sink, mut frames) = channel_sink();
    send_oauth_import(&actor.commands(), sink, "import-codex-before-heal", "codex").await;
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("initial Codex import deadline")
        .expect("initial Codex import response")
    {
        WireFrame::Response {
            body: ResponseBody::AccountOAuthImport { descriptor, .. },
            ..
        } => descriptor,
        other => panic!("unexpected initial Codex import response: {other:?}"),
    }
}

async fn import_claude_for_heal(actor: &AccountActorHandle) -> CredentialDescriptor {
    let (sink, mut frames) = channel_sink();
    send_oauth_import(
        &actor.commands(),
        sink,
        "import-claude-before-heal",
        "claude-code",
    )
    .await;
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("initial Claude import deadline")
        .expect("initial Claude import response")
    {
        WireFrame::Response {
            body: ResponseBody::AccountOAuthImport { descriptor, .. },
            ..
        } => descriptor,
        other => panic!("unexpected initial Claude import response: {other:?}"),
    }
}

/// Ages the STORED bundle past its expiry WITHOUT marking the account:
/// the natural-expiry precondition (snapshot Current), where the refresh
/// fallback stays legal (W5g-8 safety split: a snapshot-EXPIRED mark may
/// record an UNCERTAIN refresh, and under it the rotating token is never
/// replayed).
fn age_import_bundle(vault: &MemoryVault, descriptor: &CredentialDescriptor) {
    let stored = vault.resolve(&descriptor.alias).expect("imported bundle");
    let bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode imported bundle");
    let mut aged = haider_accounts::OAuthTokenBundleV1::new(
        bundle.provider_id.clone(),
        bundle.issuer.clone(),
        bundle.audience.clone(),
        bundle.resource.clone(),
        bundle.token_type.clone(),
        zeroize::Zeroizing::new(bundle.access_token().to_vec()),
        bundle
            .refresh_token()
            .map(|token| zeroize::Zeroizing::new(token.to_vec())),
        1_600_000_000_000, // 2020 — thoroughly expired
        bundle.refresh_expires_at_unix_ms,
        bundle.granted_scopes.clone(),
        bundle.identity.clone(),
        bundle.generation,
    )
    .expect("aged bundle");
    if let Some(fingerprint) = bundle.import_source_access_fingerprint() {
        aged = aged.with_import_source_access_fingerprint(fingerprint);
    }
    vault
        .put(
            &descriptor.alias,
            &aged.encode().expect("encode aged bundle"),
        )
        .expect("store aged bundle");
}

async fn mark_import_expired(
    actor: &AccountActorHandle,
    vault: &MemoryVault,
    refresh_fences: &RefreshFenceRegistry,
    descriptor: &CredentialDescriptor,
) {
    let stored = vault.resolve(&descriptor.alias).expect("imported bundle");
    let bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode imported bundle");
    let (completed, result) = tokio::sync::oneshot::channel();
    actor
        .commands()
        .send(AccountCommand::ExpireOAuthRefresh {
            descriptor: descriptor.clone(),
            expected: OAuthRefreshFence {
                fence_epoch: refresh_fences.current(&descriptor.alias),
                generation: bundle.generation,
                issuer: bundle.issuer,
                audience: bundle.audience,
                resource: bundle.resource,
                subject_hash: bundle.identity.subject_hash,
            },
            completed,
        })
        .await
        .expect("mark imported credential expired");
    assert!(
        result
            .await
            .expect("expiration response")
            .expect("expiration")
    );
}

fn openai_import_test_bundle(
    access_token: &[u8],
    refresh_token: &[u8],
    generation: u64,
) -> haider_accounts::OAuthTokenBundleV1 {
    haider_accounts::OAuthTokenBundleV1::new(
        OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        "https://auth.openai.com".into(),
        "app_EMoamEEZ73f0CkXaXp7hrann".into(),
        None,
        "Bearer".into(),
        Zeroizing::new(access_token.to_vec()),
        Some(Zeroizing::new(refresh_token.to_vec())),
        u64::MAX - 1,
        None,
        ["openid", "profile", "email", "offline_access"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        haider_accounts::OAuthIdentityV1 {
            subject_hash: blake3::hash(b"fake-account-1").to_hex().to_string(),
            display_identity: "fake-account-1".into(),
        },
        generation,
    )
    .expect("OpenAI import test bundle")
}

fn start_file_owned_claude_refresh_actor(
    snapshot: &AccountsSnapshot,
    vault: Arc<dyn Vault>,
) -> mpsc::Sender<AccountCommand> {
    let (sender, mut receiver) = mpsc::channel(8);
    let snapshot = Arc::clone(snapshot);
    tokio::spawn(async move {
        while let Some(command) = receiver.recv().await {
            match command {
                AccountCommand::BeginOAuthImportHeal { completed, .. } => {
                    let _ = completed.send(Ok(OAuthImportHealResult::RefreshFallback {
                        source: "claude-code".to_owned(),
                    }));
                }
                AccountCommand::BeginOAuthRefresh {
                    descriptor,
                    completed,
                    ..
                } => {
                    if let Ok(mut descriptors) = snapshot.lock()
                        && let Some(current) = descriptors
                            .iter_mut()
                            .find(|current| current.alias == descriptor.alias)
                    {
                        current.status = CredentialStatus::Expired;
                    }
                    let _ = completed.send(Ok(true));
                }
                AccountCommand::ApplyOAuthRefresh {
                    descriptor,
                    encoded_bundle,
                    completed,
                    ..
                } => {
                    let result = vault
                        .put(&descriptor.alias, &encoded_bundle)
                        .map_err(|_| RefreshApplyError::Persist)
                        .and_then(|()| {
                            snapshot
                                .lock()
                                .map_err(|_| RefreshApplyError::Persist)
                                .and_then(|mut descriptors| {
                                    let current = descriptors
                                        .iter_mut()
                                        .find(|current| current.alias == descriptor.alias)
                                        .ok_or(RefreshApplyError::Stale)?;
                                    current.status = descriptor.status.clone();
                                    Ok(())
                                })
                        });
                    let _ = completed.send(result);
                }
                _ => {}
            }
        }
    });
    sender
}

/// LAW (a) + (b): an expired Claude Code snapshot is replaced from the live
/// owner store, reactivated, and returned without spending the superseded
/// snapshot refresh token at Anthropic's token endpoint.
///
/// MUTATION CHECK: force `handle_oauth_import_heal` to return
/// `RefreshFallback` while `claude_native_owner` is true. Expected runtime
/// failure: the returned access token is the fake grant token and the endpoint
/// call count changes from zero to one.
#[tokio::test(flavor = "current_thread")]
async fn expired_claude_snapshot_reads_through_live_owner_without_refresh_grant() {
    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let native = Arc::new(StubAccountClaudeNative::with_bytes(CLAUDE_IMPORT_FIXTURE));
    let exchange = Arc::new(CountingAnthropicRefreshExchange::new());
    let native_service: Arc<dyn ClaudeNativeCredentialStore> = native.clone();
    let exchange_service: Arc<dyn OAuthRefreshExchange> = exchange.clone();
    let (mut actor, broker, snapshot, _refresh_fences) =
        start_oauth_import_heal_test_actor_with_native(
            &store,
            Arc::clone(&vault),
            anthropic_import_refresh_catalog("http://127.0.0.1:1/token"),
            native_service,
            Some(exchange_service),
        );
    let descriptor = import_claude_for_heal(&actor).await;
    assert!(descriptor.identity.ends_with("linked to Claude Code"));
    native.replace(CLAUDE_READ_THROUGH_FIXTURE);
    age_import_bundle(vault.as_ref(), &descriptor);

    let access = broker
        .resolve(&descriptor)
        .await
        .expect("live Claude owner store re-adopts the account");
    assert_eq!(
        exchange.calls(),
        0,
        "the rotated snapshot grant is never sent"
    );
    assert_eq!(access.expose_secret(), b"fake-claude-live-access-token-2");
    assert!(
        native.reads() >= 2,
        "initial import plus expiry read-through"
    );
    let stored = vault
        .resolve(&descriptor.alias)
        .and_then(|stored| haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("read-through bundle persisted");
    assert_eq!(stored.generation, 2);
    assert_eq!(stored.access_token(), b"fake-claude-live-access-token-2");
    assert_eq!(
        stored.refresh_token(),
        Some(b"fake-claude-live-refresh-token-2".as_slice())
    );
    let current = snapshot.lock().expect("snapshot after read-through");
    assert_eq!(current[0].status, CredentialStatus::Ok);
    assert!(current[0].active);
    drop(current);

    assert!(broker.shutdown().await);
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// LAW (d): without a live native owner store, a file-only Claude credential
/// remains independently refreshable and persists the rotated grant.
///
/// MUTATION CHECK: forbid every Claude refresh fallback, even when the native
/// seam returns `None`. Expected runtime failure: resolution returns the
/// re-import remedy and the endpoint call count remains zero instead of one.
#[tokio::test(flavor = "current_thread")]
async fn file_only_claude_import_uses_independent_refresh_grant_fallback() {
    let vault = Arc::new(MemoryVault::new());
    let native = Arc::new(StubAccountClaudeNative::unavailable());
    let exchange = Arc::new(CountingAnthropicRefreshExchange::new());
    assert!(
        load_claude_native_import_material(
            2,
            native.as_ref(),
            ClaudeNativeReadEvent::Significant,
        )
            .expect("native absence probe")
            .is_none()
    );
    let descriptor = CredentialDescriptor {
        alias: CredentialAlias::new(ANTHROPIC_OAUTH_PROVIDER_NAME),
        provider: ANTHROPIC_OAUTH_PROVIDER_NAME.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "Claude Max subscription · independently imported".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let bundle = haider_accounts::OAuthTokenBundleV1::new(
        ANTHROPIC_OAUTH_PROVIDER_NAME.to_owned(),
        "https://claude.ai".to_owned(),
        crate::oauth::CLAUDE_DEFAULT_CLIENT_ID.to_owned(),
        None,
        "Bearer".to_owned(),
        Zeroizing::new(b"fake-file-only-claude-access".to_vec()),
        Some(Zeroizing::new(b"fake-claude-refresh-token-1".to_vec())),
        1_600_000_000_000,
        None,
        vec!["user:inference".to_owned()],
        haider_accounts::OAuthIdentityV1 {
            subject_hash: "fake-file-only-claude-subject".to_owned(),
            display_identity: descriptor.identity.clone(),
        },
        1,
    )
    .expect("file-only Claude bundle");
    vault
        .put(
            &descriptor.alias,
            &bundle.encode().expect("encode file-only bundle"),
        )
        .expect("seed file-only bundle");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(vec![descriptor.clone()]));
    let commands =
        start_file_owned_claude_refresh_actor(&snapshot, Arc::clone(&vault) as Arc<dyn Vault>);
    let exchange_service: Arc<dyn OAuthRefreshExchange> = exchange.clone();
    let broker = CredentialBroker::new(
        Arc::clone(&vault) as Arc<dyn Vault>,
        anthropic_import_refresh_catalog("http://127.0.0.1:1/token"),
        Arc::clone(&snapshot),
        commands,
    )
    .and_then(|broker| broker.with_refresh_exchange(exchange_service))
    .expect("file-owned Claude broker");

    let access = broker
        .resolve(&descriptor)
        .await
        .expect("file-only Claude import owns its refresh rotation");
    assert_eq!(access.expose_secret(), b"fake-anthropic-grant-access-token");
    assert_eq!(exchange.calls(), 1);
    assert_eq!(
        exchange.refresh_token_fingerprints(),
        [*blake3::hash(b"fake-claude-refresh-token-1").as_bytes()]
    );
    assert_eq!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Ok
    );

    assert!(broker.shutdown().await);
}

/// MUTATION CHECK: restore the marked-expired short-circuit in
/// `CredentialBroker::resolve`. Expected runtime failure: resolution returns
/// `expired_or_replaced`, so the fresher source is neither receipt-committed
/// nor returned.
#[tokio::test(flavor = "current_thread")]
async fn marked_expired_import_commits_a_fresher_source_and_resolves_it() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("codex-auth.json");
    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_1).expect("write stale Codex fixture");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::marked_expired_import_commits_a_fresher_source_and_resolves_it",
        &[("HAIDER_CODEX_AUTH_PATH", &source_path)],
    ) {
        return;
    }
    let source_path = std::path::PathBuf::from(
        std::env::var_os("HAIDER_CODEX_AUTH_PATH").expect("isolated Codex path"),
    );
    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, broker, snapshot, refresh_fences) = start_oauth_import_heal_test_actor(
        &store,
        Arc::clone(&vault),
        import_refresh_catalog("http://127.0.0.1:1/token"),
    );
    let descriptor = import_codex_for_heal(&actor).await;
    mark_import_expired(&actor, vault.as_ref(), &refresh_fences, &descriptor).await;
    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_2).expect("write fresh Codex fixture");
    reset_oauth_import_read_count();

    let access = broker
        .resolve(&descriptor)
        .await
        .expect("fresher source self-heals");
    assert_eq!(access.expose_secret(), b"fake-access-token-2");
    assert_eq!(oauth_import_read_count(), 1);
    let stored = vault.resolve(&descriptor.alias).expect("healed bundle");
    let bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode healed bundle");
    assert_eq!(bundle.generation, 2);
    assert_eq!(bundle.access_token(), b"fake-access-token-2");
    let receipts = store.account_add_receipts().await.expect("import receipts");
    assert_eq!(receipts.len(), 2);
    assert!(receipts.iter().any(|receipt| {
        receipt.command_id.starts_with("oauth-heal-") && receipt.state == "committed"
    }));
    assert!(snapshot.lock().expect("snapshot")[0].status == CredentialStatus::Ok);

    assert!(broker.shutdown().await);
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: drop the own-refresh fallback after a same-token source
/// read, fail to carry the import-source fingerprint through rotation, or
/// treat the unchanged auth.json predecessor as fresher on the next due
/// boundary, or erase import provenance when an actor observes that a winner
/// already advanced the vault. Expected RUNTIME failure: resolution
/// fails/returns generation one's access, a stale contender receives
/// `NotImported`, the second endpoint call is absent, or its refresh-token
/// fingerprint is generation one instead of the durable rotated token.
#[tokio::test(flavor = "current_thread")]
async fn expired_imported_bundle_refreshes_instead_of_terminal_exit70() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("codex-auth.json");
    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_1).expect("write stale Codex fixture");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::expired_imported_bundle_refreshes_instead_of_terminal_exit70",
        &[("HAIDER_CODEX_AUTH_PATH", &source_path)],
    ) {
        return;
    }
    let server = ImportRefreshServer::start(ImportRefreshMode::Success, false).await;
    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, broker, snapshot, refresh_fences) =
        start_oauth_import_heal_test_actor(&store, Arc::clone(&vault), server.catalog());
    let descriptor = import_codex_for_heal(&actor).await;
    let observed_generation_one = vault
        .resolve(&descriptor.alias)
        .expect("generation-one import");
    let observed_generation_one =
        haider_accounts::OAuthTokenBundleV1::decode(observed_generation_one.expose_secret())
            .expect("observed generation-one import");
    age_import_bundle(vault.as_ref(), &descriptor);
    reset_oauth_import_read_count();

    let access = broker
        .resolve(&descriptor)
        .await
        .expect("stale source falls back to refresh");
    assert_eq!(access.expose_secret(), b"fake-refreshed-access-token");
    assert_eq!(oauth_import_read_count(), 1);
    assert_eq!(server.calls(), 1);
    let stored = vault.resolve(&descriptor.alias).expect("refreshed bundle");
    let bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode refreshed bundle");
    assert_eq!(bundle.generation, 2);
    assert_eq!(
        bundle.import_source_access_fingerprint(),
        Some(*blake3::hash(b"fake-access-token-1").as_bytes())
    );
    assert_eq!(
        bundle.refresh_token(),
        Some(b"fake-rotated-refresh-token".as_slice())
    );
    assert!(snapshot.lock().expect("snapshot")[0].status == CredentialStatus::Ok);

    // Model the actor-mailbox race directly: a contender read generation one
    // before the winner refreshed, but the actor handles its provenance probe
    // only after generation two is durable. The receipt still proves this is
    // a Codex import, so the broker must be routed to serialized adoption.
    let (completed, result) = tokio::sync::oneshot::channel();
    actor
        .commands()
        .send(AccountCommand::BeginOAuthImportHeal {
            descriptor: descriptor.clone(),
            expected: OAuthRefreshFence {
                fence_epoch: refresh_fences.current(&descriptor.alias),
                generation: observed_generation_one.generation,
                issuer: observed_generation_one.issuer,
                audience: observed_generation_one.audience,
                resource: observed_generation_one.resource,
                subject_hash: observed_generation_one.identity.subject_hash,
            },
            completed,
        })
        .await
        .expect("send stale contender provenance probe");
    assert!(matches!(
        result.await.expect("stale contender completion").expect("probe"),
        OAuthImportHealResult::RefreshFallback { source } if source == "codex"
    ));

    // auth.json intentionally remains generation one. The next lifecycle
    // refresh must recognize it as the already-imported predecessor and use
    // durable R2, never commit/replay A1/R1 over generation two.
    age_import_bundle(vault.as_ref(), &descriptor);
    let access = broker
        .resolve(&descriptor)
        .await
        .expect("stale predecessor cannot roll back rotation");
    assert_eq!(access.expose_secret(), b"fake-refreshed-access-token");
    assert_eq!(oauth_import_read_count(), 2);
    assert_eq!(server.calls(), 2);
    assert_eq!(
        server.refresh_token_fingerprints(),
        [
            *blake3::hash(b"fake-refresh-token-1").as_bytes(),
            *blake3::hash(b"fake-rotated-refresh-token").as_bytes(),
        ]
    );
    let stored = vault
        .resolve(&descriptor.alias)
        .and_then(|stored| haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("second durable imported refresh");
    assert_eq!(stored.generation, 3);
    assert_eq!(
        stored.import_source_access_fingerprint(),
        Some(*blake3::hash(b"fake-access-token-1").as_bytes())
    );
    assert_eq!(
        stored.refresh_token(),
        Some(b"fake-rotated-refresh-token".as_slice())
    );

    assert!(broker.shutdown().await);
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: restore the generic refresh failure after both healing
/// paths lose. Expected runtime failure: the exact named recovery taxonomy
/// below becomes `OAuth refresh permanently failed`/`provider_error`.
#[tokio::test(flavor = "current_thread")]
async fn failed_import_healing_names_the_import_recovery_command() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("codex-auth.json");
    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_1).expect("write stale Codex fixture");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::failed_import_healing_names_the_import_recovery_command",
        &[("HAIDER_CODEX_AUTH_PATH", &source_path)],
    ) {
        return;
    }
    let server = ImportRefreshServer::start(ImportRefreshMode::InvalidGrant, false).await;
    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, broker, _snapshot, refresh_fences) =
        start_oauth_import_heal_test_actor(&store, Arc::clone(&vault), server.catalog());
    let descriptor = import_codex_for_heal(&actor).await;
    mark_import_expired(&actor, vault.as_ref(), &refresh_fences, &descriptor).await;
    reset_oauth_import_read_count();

    let error = broker
        .resolve(&descriptor)
        .await
        .expect_err("both import healing paths lose");
    assert_eq!(error.code, ErrorCode::Unauthorized);
    assert_eq!(
        error.message,
        "credential expired — re-run `haider import codex` or sign in again"
    );
    assert_eq!(oauth_import_read_count(), 1);
    // The MARK may record an UNCERTAIN refresh (forced shutdown
    // mid-exchange): the rotating token is never replayed under it, so
    // the endpoint is never touched (W5g-8 safety split).
    assert_eq!(server.calls(), 0);

    assert!(broker.shutdown().await);
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: remove the refresh-key flight around source-first healing.
/// Expected runtime failure: both concurrent resolves re-read the source and
/// call the gated token endpoint, taking both counters above one.
#[tokio::test(flavor = "current_thread")]
async fn concurrent_import_healing_reads_source_and_refreshes_once() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("codex-auth.json");
    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_1).expect("write stale Codex fixture");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::concurrent_import_healing_reads_source_and_refreshes_once",
        &[("HAIDER_CODEX_AUTH_PATH", &source_path)],
    ) {
        return;
    }
    let server = ImportRefreshServer::start(ImportRefreshMode::Success, true).await;
    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, broker, _snapshot, refresh_fences) =
        start_oauth_import_heal_test_actor(&store, Arc::clone(&vault), server.catalog());
    let descriptor = import_codex_for_heal(&actor).await;
    let _ = &refresh_fences;
    age_import_bundle(vault.as_ref(), &descriptor);
    reset_oauth_import_read_count();

    let first = tokio::spawn({
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        async move { broker.resolve(&descriptor).await }
    });
    let second = tokio::spawn({
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        async move { broker.resolve(&descriptor).await }
    });
    server.wait_for_calls(1).await;
    assert_eq!(oauth_import_read_count(), 1);
    assert_eq!(server.calls(), 1);
    server.release();
    for task in [first, second] {
        let access = task
            .await
            .expect("resolve joins")
            .expect("resolve succeeds");
        assert_eq!(access.expose_secret(), b"fake-refreshed-access-token");
    }
    assert_eq!(oauth_import_read_count(), 1);
    assert_eq!(server.calls(), 1);

    assert!(broker.shutdown().await);
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: let any historical import receipt prove source ownership.
/// Expected runtime failure: the old Codex receipt replaces the later
/// loopback-PKCE incarnation at `openai-oauth` instead of selecting
/// `openai-oauth-2`.
#[tokio::test]
async fn oauth_import_source_ownership_tracks_the_latest_alias_incarnation() {
    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let mut accounts = memory_accounts();
    let alias = CredentialAlias::new(OPENAI_OAUTH_PROVIDER_NAME);
    let descriptor = oauth_descriptor_for(
        OPENAI_OAUTH_PROVIDER_NAME,
        &alias,
        &openai_import_test_bundle(b"fake-access-token-1", b"fake-refresh-token-1", 1),
        true,
    );
    accounts.add(descriptor.clone()).expect("seed descriptor");

    let imported = OAuthImportIdentity {
        source: "codex".to_owned(),
        alias: alias.as_str().to_owned(),
        provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        candidate: None,
    };
    let imported_json = imported.canonical_json().expect("import coordinates");
    let imported_digest = blake3::hash(imported_json.as_bytes()).to_hex().to_string();
    assert_eq!(
        store
            .account_add_claim_receipt("old-import".to_owned(), imported_digest, imported_json,)
            .await
            .expect("claim old import"),
        AccountAddClaim::Fresh
    );
    assert_eq!(
        store
            .finalize_account_add_receipt(
                "old-import".to_owned(),
                AccountAddReceiptResponse {
                    descriptor: descriptor.clone(),
                },
            )
            .await
            .expect("finalize old import"),
        1
    );

    let loopback = OAuthAddIdentity {
        provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        display_alias: alias.as_str().to_owned(),
        physical_alias: alias.as_str().to_owned(),
        auth_method: "oauth".to_owned(),
    };
    let loopback_json = loopback.canonical_json().expect("loopback coordinates");
    let loopback_digest = blake3::hash(loopback_json.as_bytes()).to_hex().to_string();
    assert_eq!(
        store
            .account_add_claim_receipt("later-loopback".to_owned(), loopback_digest, loopback_json,)
            .await
            .expect("claim loopback add"),
        AccountAddClaim::Fresh
    );
    assert_eq!(
        store
            .finalize_account_add_receipt(
                "later-loopback".to_owned(),
                AccountAddReceiptResponse { descriptor },
            )
            .await
            .expect("finalize loopback add"),
        2
    );

    let selected = select_oauth_import_alias(
        &store,
        &accounts,
        "new-import",
        "codex",
        OPENAI_OAUTH_PROVIDER_NAME,
        OPENAI_OAUTH_PROVIDER_NAME,
    )
    .await
    .expect("select alias");
    assert_eq!(selected.as_str(), "openai-oauth-2");

    store.close().await.expect("close");
}

/// LAW (c): a live Claude owner credential remains discoverable beside an
/// existing same-provider account, and the device action re-adopts the
/// expired default alias in place as active/Ok.
///
/// MUTATION CHECK: filter discovery by existing provider, or remove the
/// expired-default reuse branch in `select_oauth_import_alias`. Expected
/// runtime failure: the candidate disappears or commits as
/// `anthropic-oauth-2` instead of healing `anthropic-oauth`.
#[tokio::test(flavor = "current_thread")]
async fn claude_device_candidate_resurfaces_and_re_adopts_existing_expired_account() {
    let fixture_home = test_store_dir();
    let missing_file = fixture_home.path().join("missing-claude-credentials.json");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::claude_device_candidate_resurfaces_and_re_adopts_existing_expired_account",
        &[
            ("HOME", fixture_home.path()),
            ("HAIDER_CLAUDE_CREDS_PATH", &missing_file),
        ],
    ) {
        return;
    }

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let mut accounts = memory_accounts();
    let expired = CredentialDescriptor {
        alias: CredentialAlias::new(ANTHROPIC_OAUTH_PROVIDER_NAME),
        provider: ANTHROPIC_OAUTH_PROVIDER_NAME.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "expired Claude subscription".to_owned(),
        status: CredentialStatus::Expired,
        active: true,
    };
    accounts.add(expired.clone()).expect("seed expired account");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(vec![expired]));
    let providers = test_provider_registry();
    let management = ManagementSnapshot::new(
        0,
        accounts.list().to_vec(),
        providers.summaries(&provider_has_credential(&accounts)),
    );
    let native = Arc::new(StubAccountClaudeNative::with_bytes(
        CLAUDE_READ_THROUGH_FIXTURE,
    ));
    let candidate =
        crate::device_discovery::discover_device_candidates_with_native(false, native.as_ref())
            .into_iter()
            .find(|candidate| candidate.wire.provider == ANTHROPIC_OAUTH_PROVIDER_NAME)
            .expect("same-provider Claude candidate remains surfaced");
    assert_eq!(candidate.wire.source_label, "Linked to Claude Code");

    let (sink, mut frames) = channel_sink();
    let native_service: Arc<dyn ClaudeNativeCredentialStore> = native;
    handle_device_import(
        &store,
        &mut accounts,
        Arc::clone(&vault) as Arc<dyn Vault>,
        &snapshot,
        Some(&management),
        &providers,
        &HashSet::new(),
        &RefreshFenceRegistry::default(),
        Arc::new(UnreachableGcloud),
        native_service,
        DeviceImportJob {
            command_id: "re-adopt-claude-device".to_owned(),
            candidate: candidate.wire.candidate,
            discovery_disabled: false,
            route: LoginRoute {
                request_id: RequestId::new("re-adopt-claude-device-request"),
                sink,
            },
        },
    )
    .await;
    let descriptor = match frames.try_recv().expect("re-adopt response") {
        WireFrame::Response {
            body: ResponseBody::AccountImportDevice { descriptor, .. },
            ..
        } => descriptor,
        other => panic!("unexpected re-adopt response: {other:?}"),
    };
    assert_eq!(descriptor.alias.as_str(), ANTHROPIC_OAUTH_PROVIDER_NAME);
    assert_eq!(descriptor.status, CredentialStatus::Ok);
    assert!(descriptor.active);
    assert!(descriptor.identity.ends_with("linked to Claude Code"));
    assert_eq!(accounts.list().len(), 1);
    assert_eq!(snapshot.lock().expect("snapshot")[0], descriptor);

    store.close().await.expect("close");
}

/// MUTATION CHECK: remove the expired-default repair branch in
/// `select_oauth_import_alias`. Expected runtime failure: the discovered
/// Claude credential is assigned `anthropic-oauth-2` instead of superseding
/// the unusable `anthropic-oauth` slot.
#[tokio::test]
async fn oauth_import_repairs_an_expired_default_alias_in_place() {
    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let mut accounts = memory_accounts();
    let alias = CredentialAlias::new(ANTHROPIC_OAUTH_PROVIDER_NAME);
    let descriptor = CredentialDescriptor {
        alias,
        provider: ANTHROPIC_OAUTH_PROVIDER_NAME.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "expired Claude subscription".to_owned(),
        status: CredentialStatus::Expired,
        active: true,
    };
    accounts
        .add(descriptor)
        .expect("seed expired Claude account");

    let selected = select_oauth_import_alias(
        &store,
        &accounts,
        "repair-expired-claude",
        "claude-code",
        ANTHROPIC_OAUTH_PROVIDER_NAME,
        ANTHROPIC_OAUTH_PROVIDER_NAME,
    )
    .await
    .expect("select expired default alias");
    assert_eq!(selected.as_str(), ANTHROPIC_OAUTH_PROVIDER_NAME);

    store.close().await.expect("close");
}

/// MUTATION CHECK: stamp the Codex import with a non-sanctioned issuer or
/// serialize a token into its receipt. Expected runtime failure: the bundle
/// metadata or secret-free receipt assertions below fail after the command
/// has crossed the real actor/vault/revision seam.
#[tokio::test(flavor = "current_thread")]
async fn codex_import_commits_refreshable_bundle_and_secret_free_receipt() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("codex-auth.json");
    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_1).expect("write Codex fixture");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::codex_import_commits_refreshable_bundle_and_secret_free_receipt",
        &[("HAIDER_CODEX_AUTH_PATH", &source_path)],
    ) {
        return;
    }

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, snapshot, management) = start_oauth_import_test_actor(
        &store,
        Arc::clone(&vault),
        HashSet::new(),
        RefreshFenceRegistry::default(),
    );
    let (sink, mut frames) = channel_sink();
    send_oauth_import(&actor.commands(), sink, "import-codex-1", "codex").await;
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("Codex import response deadline")
        .expect("Codex import response");
    let descriptor = match frame {
        WireFrame::Response {
            body:
                ResponseBody::AccountOAuthImport {
                    descriptor,
                    revision,
                },
            ..
        } => {
            assert_eq!(revision, 1);
            descriptor
        }
        other => panic!("unexpected Codex import response: {other:?}"),
    };
    assert_eq!(descriptor.alias.as_str(), OPENAI_OAUTH_PROVIDER_NAME);
    assert_eq!(descriptor.provider, OPENAI_OAUTH_PROVIDER_NAME);
    assert_eq!(descriptor.identity, "fake-account-1");
    assert!(descriptor.active);
    {
        let current = snapshot.lock().expect("snapshot");
        assert_eq!(current.len(), 1);
        assert_eq!(current[0], descriptor);
    }
    assert_eq!(management.read().expect("management").revision, 1);

    let stored = vault
        .resolve(&CredentialAlias::new(OPENAI_OAUTH_PROVIDER_NAME))
        .expect("stored Codex bundle");
    let bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode Codex bundle");
    assert_eq!(bundle.provider_id, OPENAI_OAUTH_PROVIDER_NAME);
    assert_eq!(bundle.issuer, "https://auth.openai.com");
    assert_eq!(bundle.audience, "app_EMoamEEZ73f0CkXaXp7hrann");
    assert!(bundle.token_type.eq_ignore_ascii_case("bearer"));
    assert_eq!(bundle.access_token(), b"fake-access-token-1");
    assert_eq!(
        bundle.refresh_token(),
        Some(b"fake-refresh-token-1".as_slice())
    );
    assert_eq!(bundle.generation, 1);
    assert_eq!(
        bundle.import_source_access_fingerprint(),
        Some(*blake3::hash(b"fake-access-token-1").as_bytes())
    );
    assert!(
        bundle.refresh_on_first_use(),
        "fallback refresh marker was not retained in the vault bundle"
    );
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock after epoch")
        .as_millis();
    let now_ms = u64::try_from(now_ms).expect("test clock fits u64 milliseconds");
    assert!(
        bundle.expires_at_unix_ms >= now_ms.saturating_add(13 * 60 * 1000)
            && bundle.expires_at_unix_ms <= now_ms.saturating_add(16 * 60 * 1000),
        "non-JWT fake token did not receive the short refresh-on-first-use fallback"
    );
    assert!(
        ["openid", "profile", "email", "offline_access"]
            .into_iter()
            .all(|scope| bundle.granted_scopes.iter().any(|granted| granted == scope))
    );

    let receipts = store.account_add_receipts().await.expect("import receipts");
    let receipt = receipts
        .iter()
        .find(|row| row.command_id == "import-codex-1")
        .expect("Codex receipt");
    assert_eq!(
        receipt.request_json,
        r#"{"source":"codex","alias":"openai-oauth","provider":"openai-oauth"}"#
    );
    let durable = format!(
        "{}{}",
        receipt.request_json,
        receipt.response_json.as_deref().unwrap_or_default()
    );
    for secret in [
        "fake-access-token-1",
        "fake-refresh-token-1",
        "fake-id-token-1",
    ] {
        assert!(!durable.contains(secret), "receipt leaked {secret}");
    }

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: ignore Claude Code's `expiresAt` and stamp a fallback.
/// Expected runtime failure: the decoded bundle expiry differs from the
/// exact millisecond value in the daemon-read fixture.
#[tokio::test(flavor = "current_thread")]
async fn claude_code_import_honors_expiry_and_anthropic_registration() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("claude-credentials.json");
    std::fs::write(&source_path, CLAUDE_IMPORT_FIXTURE).expect("write Claude fixture");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::claude_code_import_honors_expiry_and_anthropic_registration",
        &[("HAIDER_CLAUDE_CREDS_PATH", &source_path)],
    ) {
        return;
    }

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, snapshot, management) = start_oauth_import_test_actor(
        &store,
        Arc::clone(&vault),
        HashSet::new(),
        RefreshFenceRegistry::default(),
    );
    let (sink, mut frames) = channel_sink();
    send_oauth_import(&actor.commands(), sink, "import-claude-1", "claude-code").await;
    let descriptor = match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("Claude import response deadline")
        .expect("Claude import response")
    {
        WireFrame::Response {
            body:
                ResponseBody::AccountOAuthImport {
                    descriptor,
                    revision: 1,
                },
            ..
        } => descriptor,
        other => panic!("unexpected Claude import response: {other:?}"),
    };
    assert_eq!(descriptor.alias.as_str(), ANTHROPIC_OAUTH_PROVIDER_NAME);
    assert_eq!(descriptor.provider, ANTHROPIC_OAUTH_PROVIDER_NAME);
    assert_eq!(
        descriptor.identity,
        "Claude Max subscription · independently imported"
    );
    assert!(descriptor.active);
    assert_eq!(snapshot.lock().expect("snapshot").len(), 1);
    assert_eq!(management.read().expect("management").revision, 1);
    let stored = vault
        .resolve(&CredentialAlias::new(ANTHROPIC_OAUTH_PROVIDER_NAME))
        .expect("stored Claude bundle");
    let bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode Claude bundle");
    assert_eq!(bundle.provider_id, ANTHROPIC_OAUTH_PROVIDER_NAME);
    assert_eq!(bundle.issuer, "https://claude.ai");
    assert_eq!(bundle.audience, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(bundle.expires_at_unix_ms, 4_102_444_800_123);
    assert_eq!(bundle.access_token(), b"fake-claude-access-token-1");
    assert_eq!(
        bundle.import_source_access_fingerprint(),
        Some(*blake3::hash(b"fake-claude-access-token-1").as_bytes())
    );
    assert_eq!(
        bundle.refresh_token(),
        Some(b"fake-claude-refresh-token-1".as_slice())
    );
    assert_eq!(bundle.granted_scopes, ["user:inference"]);

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK (W5g-7): make `validation_required` return `false` for
/// `user:inference` too (validation demands nothing). Expected runtime
/// failure: the inference-less grant below imports instead of being
/// refused — a credential the turn path can never use.
#[tokio::test(flavor = "current_thread")]
async fn claude_code_import_without_inference_scope_is_refused() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("claude-credentials.json");
    std::fs::write(
        &source_path,
        br#"{
  "claudeAiOauth": {
    "accessToken": "fake-claude-access-token-9",
    "refreshToken": "fake-claude-refresh-token-9",
    "expiresAt": 4102444800123,
    "scopes": ["user:profile"],
    "subscriptionType": "max"
  }
}"#,
    )
    .expect("write scopeless Claude fixture");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::claude_code_import_without_inference_scope_is_refused",
        &[("HAIDER_CLAUDE_CREDS_PATH", &source_path)],
    ) {
        return;
    }

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, snapshot, _management) = start_oauth_import_test_actor(
        &store,
        Arc::clone(&vault),
        HashSet::new(),
        RefreshFenceRegistry::default(),
    );
    let (sink, mut frames) = channel_sink();
    send_oauth_import(&actor.commands(), sink, "import-claude-9", "claude-code").await;
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("refusal deadline")
        .expect("refusal response")
    {
        WireFrame::Response {
            body: ResponseBody::Error { code, .. },
            ..
        } => assert_eq!(code, ERROR_CODE_INVALID_ARGUMENT),
        other => panic!("an inference-less grant must be refused: {other:?}"),
    }
    assert_eq!(snapshot.lock().expect("snapshot").len(), 0, "nothing lands");

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: downgrade a missing/malformed import into a partial
/// commit. Expected runtime failure: either the descriptor snapshot, vault
/// list, or management revision changes despite the path-naming error.
#[tokio::test(flavor = "current_thread")]
async fn malformed_and_missing_imports_name_paths_and_commit_nothing() {
    let fixture_dir = test_store_dir();
    let malformed_path = fixture_dir.path().join("malformed-codex.json");
    std::fs::write(&malformed_path, br#"{"tokens":"not-an-object"}"#)
        .expect("write malformed fixture");
    let missing_path = fixture_dir.path().join("missing-claude.json");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::malformed_and_missing_imports_name_paths_and_commit_nothing",
        &[
            ("HAIDER_CODEX_AUTH_PATH", &malformed_path),
            ("HAIDER_CLAUDE_CREDS_PATH", &missing_path),
        ],
    ) {
        return;
    }
    let malformed_path = std::path::PathBuf::from(
        std::env::var_os("HAIDER_CODEX_AUTH_PATH").expect("isolated Codex path"),
    );
    let missing_path = std::path::PathBuf::from(
        std::env::var_os("HAIDER_CLAUDE_CREDS_PATH").expect("isolated Claude path"),
    );

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, snapshot, management) = start_oauth_import_test_actor(
        &store,
        Arc::clone(&vault),
        HashSet::new(),
        RefreshFenceRegistry::default(),
    );
    let (sink, mut frames) = channel_sink();
    send_oauth_import(
        &actor.commands(),
        Arc::clone(&sink),
        "import-malformed",
        "codex",
    )
    .await;
    let malformed_message = match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("malformed response deadline")
        .expect("malformed response")
    {
        WireFrame::Response {
            body: ResponseBody::Error { message, .. },
            ..
        } => message,
        other => panic!("malformed import unexpectedly succeeded: {other:?}"),
    };
    assert!(malformed_message.contains(&malformed_path.display().to_string()));

    send_oauth_import(&actor.commands(), sink, "import-missing", "claude-code").await;
    let missing_message = match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("missing response deadline")
        .expect("missing response")
    {
        WireFrame::Response {
            body: ResponseBody::Error { message, .. },
            ..
        } => message,
        other => panic!("missing import unexpectedly succeeded: {other:?}"),
    };
    assert!(missing_message.contains(&missing_path.display().to_string()));
    assert!(snapshot.lock().expect("snapshot").is_empty());
    assert!(vault.list().expect("vault list").is_empty());
    assert_eq!(management.read().expect("management").revision, 0);

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: skip the import's pending-login reservation check.
/// Expected runtime failure: the command commits `openai-oauth` over the
/// durable in-flight login instead of returning retryable `busy`.
#[tokio::test(flavor = "current_thread")]
async fn oauth_import_obeys_the_reserved_alias_fence() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("codex-auth.json");
    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_1).expect("write Codex fixture");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::oauth_import_obeys_the_reserved_alias_fence",
        &[("HAIDER_CODEX_AUTH_PATH", &source_path)],
    ) {
        return;
    }

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let login_identity = LoginIdentity {
        provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        resolved_model: "gpt-test".to_owned(),
        display_alias: Some(OPENAI_OAUTH_PROVIDER_NAME.to_owned()),
        physical_alias: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
    };
    let login_json = login_identity.canonical_json().expect("login coordinates");
    let login_digest = blake3::hash(login_json.as_bytes()).to_hex().to_string();
    assert_eq!(
        store
            .login_claim_receipt(
                "pending-login-reservation".to_owned(),
                login_digest,
                login_json,
            )
            .await
            .expect("claim pending login"),
        LoginClaim::Fresh
    );
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, snapshot, management) = start_oauth_import_test_actor(
        &store,
        Arc::clone(&vault),
        HashSet::new(),
        RefreshFenceRegistry::default(),
    );
    let (sink, mut frames) = channel_sink();
    send_oauth_import(&actor.commands(), sink, "import-reserved", "codex").await;
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("reserved response deadline")
        .expect("reserved response")
    {
        WireFrame::Response {
            body: ResponseBody::Error {
                code, retryable, ..
            },
            ..
        } => {
            assert_eq!(code, ERROR_CODE_BUSY);
            assert!(retryable);
        }
        other => panic!("reserved import unexpectedly succeeded: {other:?}"),
    }
    assert!(snapshot.lock().expect("snapshot").is_empty());
    assert!(vault.list().expect("vault list").is_empty());
    assert_eq!(management.read().expect("management").revision, 0);

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: remove `refresh_fences.invalidate(&alias)` from re-import.
/// Expected runtime failure: the epoch-zero late refresh below overwrites the
/// generation-two imported bundle instead of returning `Stale`; changing the
/// replacement to active also fails the active-slot assertions.
#[tokio::test(flavor = "current_thread")]
async fn reimport_replaces_bundle_fences_refresh_and_preserves_other_active_account() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("codex-auth.json");
    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_1).expect("write Codex fixture 1");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::reimport_replaces_bundle_fences_refresh_and_preserves_other_active_account",
        &[("HAIDER_CODEX_AUTH_PATH", &source_path)],
    ) {
        return;
    }
    let source_path = std::path::PathBuf::from(
        std::env::var_os("HAIDER_CODEX_AUTH_PATH").expect("isolated Codex path"),
    );

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let refresh_fences = RefreshFenceRegistry::default();
    let (mut actor, snapshot, _management) =
        start_oauth_import_test_actor(&store, Arc::clone(&vault), HashSet::new(), refresh_fences);
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();
    send_oauth_import(&commands, Arc::clone(&sink), "import-codex-first", "codex").await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("first import deadline")
            .expect("first import response"),
        WireFrame::Response {
            body: ResponseBody::AccountOAuthImport { revision: 1, .. },
            ..
        }
    ));

    commands
        .send(AccountCommand::AddOAuth(Box::new(OAuthAddJob {
            command_id: "add-manual-oauth".into(),
            provider: OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
            display_alias: "manual-oauth".into(),
            claim: Some(OAuthReadyClaim::for_account_test(
                OPENAI_OAUTH_PROVIDER_NAME,
                "manual-oauth",
                openai_import_test_bundle(
                    b"fake-manual-access-token",
                    b"fake-manual-refresh-token",
                    1,
                ),
            )),
            route: LoginRoute {
                request_id: RequestId::new("add-manual-oauth-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("add manual OAuth account");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("manual add deadline")
            .expect("manual add response"),
        WireFrame::Response {
            body: ResponseBody::AccountAdd { .. },
            ..
        }
    ));
    commands
        .send(AccountCommand::SetActive(Box::new(SetActiveJob {
            command_id: "activate-manual-oauth".into(),
            alias: "manual-oauth".into(),
            route: LoginRoute {
                request_id: RequestId::new("activate-manual-oauth-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("activate manual OAuth");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("set active deadline")
            .expect("set active response"),
        WireFrame::Response {
            body: ResponseBody::AccountSetActive { revision: 3, .. },
            ..
        }
    ));

    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_2).expect("write Codex fixture 2");
    send_oauth_import(&commands, Arc::clone(&sink), "import-codex-second", "codex").await;
    let imported_descriptor = match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("second import deadline")
        .expect("second import response")
    {
        WireFrame::Response {
            body:
                ResponseBody::AccountOAuthImport {
                    descriptor,
                    revision: 4,
                },
            ..
        } => descriptor,
        other => panic!("unexpected second import response: {other:?}"),
    };
    assert_eq!(
        imported_descriptor.alias.as_str(),
        OPENAI_OAUTH_PROVIDER_NAME
    );
    assert!(!imported_descriptor.active, "re-import stole active slot");
    {
        let descriptors = snapshot.lock().expect("snapshot");
        assert!(descriptors.iter().any(|descriptor| {
            descriptor.alias.as_str() == "manual-oauth" && descriptor.active
        }));
        assert!(descriptors.iter().any(|descriptor| {
            descriptor.alias.as_str() == OPENAI_OAUTH_PROVIDER_NAME && !descriptor.active
        }));
    }
    let alias = CredentialAlias::new(OPENAI_OAUTH_PROVIDER_NAME);
    let stored = vault.resolve(&alias).expect("re-imported bundle");
    let bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode re-imported bundle");
    assert_eq!(bundle.access_token(), b"fake-access-token-2");
    assert_eq!(
        bundle.refresh_token(),
        Some(b"fake-refresh-token-2".as_slice())
    );
    assert_eq!(bundle.generation, 2);

    let late = haider_accounts::OAuthTokenBundleV1::new(
        bundle.provider_id.clone(),
        bundle.issuer.clone(),
        bundle.audience.clone(),
        bundle.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"fake-late-access-token".to_vec()),
        Some(Zeroizing::new(b"fake-late-refresh-token".to_vec())),
        u64::MAX - 1,
        None,
        bundle.granted_scopes.clone(),
        bundle.identity.clone(),
        3,
    )
    .expect("late refresh bundle");
    let (completed, applied) = tokio::sync::oneshot::channel();
    commands
        .send(AccountCommand::ApplyOAuthRefresh {
            descriptor: imported_descriptor,
            expected: OAuthRefreshFence {
                fence_epoch: 0,
                generation: bundle.generation,
                issuer: bundle.issuer.clone(),
                audience: bundle.audience.clone(),
                resource: bundle.resource.clone(),
                subject_hash: bundle.identity.subject_hash.clone(),
            },
            encoded_bundle: late.encode().expect("encode late refresh"),
            completed,
        })
        .await
        .expect("send late refresh");
    assert!(matches!(
        applied.await.expect("late refresh response"),
        Err(RefreshApplyError::Stale)
    ));
    let stored = vault.resolve(&alias).expect("bundle after stale refresh");
    let stored = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode bundle after stale refresh");
    assert_eq!(stored.access_token(), b"fake-access-token-2");
    assert_eq!(stored.generation, 2);

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK (W5f-4): remove the
/// `Ok(Err(error)) if error.code == ErrorCode::CredentialMissing => None`
/// arm from `handle_oauth_import`'s prior-secret read (let the migration
/// case fall through to the fatal `respond_management_error`). Expected
/// runtime failure: the re-import below — over a descriptor whose vault
/// secret was retired (the Keychain→file-vault upgrade) — errors instead
/// of restoring the account. Confirmed live 2026-07-30: this is the exact
/// path an upgrading user hits.
/// Verified by revert on 2026-07-30.
#[tokio::test(flavor = "current_thread")]
async fn reimport_over_a_retired_secret_restores_the_account() {
    let fixture_dir = test_store_dir();
    let source_path = fixture_dir.path().join("codex-auth.json");
    std::fs::write(&source_path, CODEX_IMPORT_FIXTURE_1).expect("write Codex fixture");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::reimport_over_a_retired_secret_restores_the_account",
        &[("HAIDER_CODEX_AUTH_PATH", &source_path)],
    ) {
        return;
    }

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, snapshot, _management) = start_oauth_import_test_actor(
        &store,
        Arc::clone(&vault),
        HashSet::new(),
        RefreshFenceRegistry::default(),
    );
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();

    // First import: descriptor + vault secret committed.
    send_oauth_import(&commands, Arc::clone(&sink), "import-pre-upgrade", "codex").await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("first import deadline")
            .expect("first import response"),
        WireFrame::Response {
            body: ResponseBody::AccountOAuthImport { revision: 1, .. },
            ..
        }
    ));

    // The upgrade: the descriptor survives, but its secret is GONE (the old
    // Keychain item the user could not identify, deleted).
    let alias = CredentialAlias::new(OPENAI_OAUTH_PROVIDER_NAME);
    vault.delete(&alias).expect("retire the prior secret");
    assert!(
        vault.resolve(&alias).is_err(),
        "the prior secret must be gone to model the migration"
    );

    // Re-import must RESTORE, not error.
    send_oauth_import(&commands, Arc::clone(&sink), "import-post-upgrade", "codex").await;
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("re-import deadline")
        .expect("re-import response")
    {
        WireFrame::Response {
            body: ResponseBody::AccountOAuthImport { descriptor, .. },
            ..
        } => {
            assert_eq!(descriptor.alias.as_str(), OPENAI_OAUTH_PROVIDER_NAME);
            assert!(descriptor.active, "the restored account stays active");
        }
        other => panic!("re-import over a retired secret must succeed, got {other:?}"),
    }
    // The secret is back in the vault, and the account list still holds one.
    assert!(
        vault.resolve(&alias).is_ok(),
        "the re-import restored the vault secret"
    );
    assert_eq!(snapshot.lock().expect("snapshot").len(), 1);

    actor.shutdown().await;
    store.close().await.expect("close");
}

fn removable_provider_profile(provider: &str) -> ProviderProfileV1 {
    ProviderProfileV1 {
        provider_id: provider.to_owned(),
        display_name: provider.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        base_url: Some("https://custom.example.invalid".to_owned()),
        enabled: true,
        auth_requirement: ProviderAuthRequirementWire::ApiKey,
        configured_models: vec!["custom-model".to_owned()],
        default_model: Some("custom-model".to_owned()),
        provenance: ProviderProvenance::Custom,
    }
}

/// A committed custom-provider removal publishes the next registry revision,
/// replays before all fresh validation, deletes the model cache, and remains
/// authoritative over a stale provider JSON projection on restart.
///
/// MUTATION CHECK: skip `ProviderRegistry::remove_profile`'s durable save or
/// the committed-remove arm in `reconcile_provider_receipts`. Expected RUNTIME
/// failure: `custom-lab` remains in the live list or is resurrected after the
/// stale projection is loaded during the simulated restart.
///
/// MUTATION CHECK: move provider-remove replay after revision/registry guards,
/// accept a changed body for the same command id, or drop the revision CAS.
/// Expected RUNTIME failure: the replay returns a conflict/not-found, the
/// changed body succeeds, or the fresh stale command does not conflict.
#[tokio::test]
async fn provider_remove_commits_replays_fences_and_beats_restart_resurrection() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let older_configure = ProviderConfigureInput {
        provider: "custom-lab".to_owned(),
        api_family: None,
        origin: None,
        auth_requirement: None,
        enabled: true,
        models: vec!["custom-model".to_owned()],
        default_model: Some("custom-model".to_owned()),
    };
    let older_identity = ProviderConfigureIdentity {
        input: older_configure.clone(),
        expected_revision: 0,
    };
    let (older_json, older_digest) =
        command_json(&older_identity).expect("older configure identity");
    assert!(matches!(
        store
            .management_claim_receipt::<ProviderReceipt>(
                "older-pending-configure".to_owned(),
                PROVIDER_CONFIGURE_METHOD.to_owned(),
                older_digest,
                older_json,
                Some(serde_json::to_string(&older_configure).expect("older recovery JSON")),
                Some(0),
            )
            .await
            .expect("claim older configure"),
        ManagementClaim::Fresh
    ));
    let model = haider_provider::DiscoveredModel {
        slug: "custom-model".to_owned(),
        display_name: "Custom Model".to_owned(),
        context_window: Some(64_000),
        description: None,
        default_effort: None,
        supported_efforts: Vec::new(),
        visible: true,
        priority: None,
        extensions: None,
    };
    store
        .put_provider_models(
            "custom-lab".to_owned(),
            serde_json::to_string(&vec![model.clone()]).expect("model cache JSON"),
            Some("custom-etag".to_owned()),
            55,
        )
        .await
        .expect("seed model cache");
    let builtin_profiles = initial_provider_profiles(
        &std::collections::BTreeSet::from([OPENAI_PROVIDER_NAME.to_owned()]),
        "unused",
    );
    let custom = removable_provider_profile("custom-lab");
    let mut initial = builtin_profiles.clone();
    initial.push(custom.clone());
    let source = Arc::new(CachedProviderModelSource::default());
    source.replace("custom-lab".to_owned(), vec![model]);
    let provider_store: Box<dyn ProviderRegistryStoreLike> =
        Box::new(JsonProviderRegistryStore::new(dir.path()));
    let providers = ProviderRegistry::new(provider_store, initial, source)
        .expect("provider registry with custom profile");
    let accounts = memory_accounts();
    let management = ManagementSnapshot::new(0, Vec::new(), providers.summaries(&|_| false));
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: Arc::new(MemoryVault::new()),
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::new(StdMutex::new(Vec::new())),
        management: Some(management.clone()),
        profile_id: "provider-remove".into(),
        default_model: "unused".into(),
        providers,
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();

    commands
        .send(AccountCommand::RemoveProvider(Box::new(
            ProviderRemoveJob {
                command_id: "remove-custom".to_owned(),
                provider: "custom-lab".to_owned(),
                expected_revision: 0,
                route: LoginRoute {
                    request_id: RequestId::new("remove-custom"),
                    sink: Arc::clone(&sink),
                },
            },
        )))
        .await
        .expect("send remove");
    assert!(matches!(
        frames.recv().await.expect("remove response"),
        WireFrame::Response {
            body: ResponseBody::ProviderRemove { provider, revision: 1 },
            ..
        } if provider == "custom-lab"
    ));
    let view = management.read().expect("management view");
    assert_eq!(view.revision, 1);
    assert!(
        view.providers
            .iter()
            .all(|provider| provider.provider != "custom-lab")
    );
    drop(view);
    assert!(
        store
            .provider_models("custom-lab".to_owned())
            .await
            .expect("cache read")
            .is_none()
    );

    commands
        .send(AccountCommand::RemoveProvider(Box::new(
            ProviderRemoveJob {
                command_id: "remove-custom".to_owned(),
                provider: "different-provider".to_owned(),
                expected_revision: 0,
                route: LoginRoute {
                    request_id: RequestId::new("changed-remove"),
                    sink: Arc::clone(&sink),
                },
            },
        )))
        .await
        .expect("send changed-body replay");
    assert!(matches!(
        frames.recv().await.expect("changed-body response"),
        WireFrame::Response {
            body: ResponseBody::Error { code, .. },
            ..
        } if code == ERROR_CODE_INVALID_ARGUMENT
    ));
    assert_eq!(
        store
            .advance_management_revision()
            .await
            .expect("later revision"),
        2
    );
    commands
        .send(AccountCommand::RemoveProvider(Box::new(
            ProviderRemoveJob {
                command_id: "remove-custom".to_owned(),
                provider: "custom-lab".to_owned(),
                expected_revision: 0,
                route: LoginRoute {
                    request_id: RequestId::new("replay-remove"),
                    sink: Arc::clone(&sink),
                },
            },
        )))
        .await
        .expect("send committed replay");
    assert!(matches!(
        frames.recv().await.expect("replay response"),
        WireFrame::Response {
            body: ResponseBody::ProviderRemove { provider, revision: 1 },
            ..
        } if provider == "custom-lab"
    ));
    commands
        .send(AccountCommand::RemoveProvider(Box::new(
            ProviderRemoveJob {
                command_id: "stale-fresh-remove".to_owned(),
                provider: "another-provider".to_owned(),
                expected_revision: 0,
                route: LoginRoute {
                    request_id: RequestId::new("stale-fresh-remove"),
                    sink,
                },
            },
        )))
        .await
        .expect("send stale fresh command");
    assert!(matches!(
        frames.recv().await.expect("revision conflict response"),
        WireFrame::Response {
            body: ResponseBody::Error {
                code,
                data: Some(ErrorData::RevisionConflict {
                    expected_revision: 0,
                    current_revision: 2,
                }),
                ..
            },
            ..
        } if code == ERROR_CODE_REVISION_CONFLICT
    ));
    actor.shutdown().await;

    let stale_store = JsonProviderRegistryStore::new(dir.path());
    let mut stale_profiles = builtin_profiles.clone();
    stale_profiles.push(custom);
    stale_store
        .save(&stale_profiles)
        .expect("plant stale provider projection");
    let restarted_store: Box<dyn ProviderRegistryStoreLike> = Box::new(stale_store);
    let mut restarted = ProviderRegistry::new(
        restarted_store,
        builtin_profiles,
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("restart loads stale projection");
    assert!(restarted.get("custom-lab").is_some());
    reconcile_provider_receipts(&store, &memory_accounts(), &mut restarted)
        .await
        .expect("removal receipt reconciles restart");
    assert!(restarted.get("custom-lab").is_none());
    store.close().await.expect("close store");
}

/// Builtin/factory profiles are release-owned, and custom providers remain
/// intact while any credential descriptor names them. Refusal data carries
/// every blocking alias in deterministic order.
///
/// MUTATION CHECK: drop either provenance guard or blocking-account guard in
/// `handle_provider_remove`. Expected RUNTIME failure: one request succeeds,
/// advances the revision, and removes a profile the assertions retain.
#[tokio::test]
async fn provider_remove_refuses_release_owned_and_account_referenced_profiles() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let mut initial = initial_provider_profiles(
        &std::collections::BTreeSet::from([
            OPENAI_PROVIDER_NAME.to_owned(),
            "factory-provider".to_owned(),
        ]),
        "unused",
    );
    initial.push(removable_provider_profile("custom-in-use"));
    let provider_store: Box<dyn ProviderRegistryStoreLike> = Box::new(TestProviderStore::default());
    let providers = ProviderRegistry::new(
        provider_store,
        initial,
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("provider registry");
    let mut accounts = memory_accounts();
    for (alias, active) in [("zeta-key", true), ("alpha-key", false)] {
        accounts
            .add(CredentialDescriptor {
                alias: CredentialAlias::new(alias),
                provider: "custom-in-use".to_owned(),
                base_url: None,
                auth_method: AuthMethod::ApiKey,
                identity: "fixture".to_owned(),
                status: CredentialStatus::Ok,
                active,
            })
            .expect("blocking descriptor");
    }
    let management =
        ManagementSnapshot::new(0, accounts.list().to_vec(), providers.summaries(&|_| false));
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: Arc::new(MemoryVault::new()),
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::new(StdMutex::new(Vec::new())),
        management: Some(management.clone()),
        profile_id: "provider-remove-refusals".into(),
        default_model: "unused".into(),
        providers,
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();

    for (command_id, provider) in [
        ("remove-builtin", OPENAI_PROVIDER_NAME),
        ("remove-factory", "factory-provider"),
    ] {
        commands
            .send(AccountCommand::RemoveProvider(Box::new(
                ProviderRemoveJob {
                    command_id: command_id.to_owned(),
                    provider: provider.to_owned(),
                    expected_revision: 0,
                    route: LoginRoute {
                        request_id: RequestId::new(command_id),
                        sink: Arc::clone(&sink),
                    },
                },
            )))
            .await
            .expect("send release-owned refusal");
        assert!(matches!(
            frames.recv().await.expect("release-owned response"),
            WireFrame::Response {
                body: ResponseBody::Error {
                    code,
                    data: Some(ErrorData::ProviderRemoveRefused {
                        reason: ProviderRemoveRefusalReasonWire::ReleaseOwned,
                        blocking_aliases,
                        ..
                    }),
                    ..
                },
                ..
            } if code == ERROR_CODE_PROVIDER_REMOVE_REFUSED && blocking_aliases.is_empty()
        ));
    }
    commands
        .send(AccountCommand::RemoveProvider(Box::new(
            ProviderRemoveJob {
                command_id: "remove-blocked-custom".to_owned(),
                provider: "custom-in-use".to_owned(),
                expected_revision: 0,
                route: LoginRoute {
                    request_id: RequestId::new("remove-blocked-custom"),
                    sink,
                },
            },
        )))
        .await
        .expect("send blocking-account refusal");
    assert!(matches!(
        frames.recv().await.expect("blocking response"),
        WireFrame::Response {
            body: ResponseBody::Error {
                code,
                data: Some(ErrorData::ProviderRemoveRefused {
                    provider,
                    reason: ProviderRemoveRefusalReasonWire::BlockingAccounts,
                    blocking_aliases,
                }),
                ..
            },
            ..
        } if code == ERROR_CODE_PROVIDER_REMOVE_REFUSED
            && provider == "custom-in-use"
            && blocking_aliases == ["alpha-key", "zeta-key"]
    ));
    let view = management.read().expect("management view");
    assert_eq!(view.revision, 0);
    assert!(
        view.providers
            .iter()
            .any(|provider| provider.provider == "custom-in-use")
    );
    drop(view);
    assert!(
        store
            .provider_management_receipts()
            .await
            .expect("receipts")
            .is_empty()
    );
    actor.shutdown().await;
    store.close().await.expect("close store");
}

/// MUTATION CHECK (W5g-5 live fix): make `custom_login_target` ignore the
/// API family (return a target for any profile with an endpoint). Expected
/// runtime failure: the builtin-responses row below yields a target, so a
/// vendor login would validate against the wrong origin.
#[test]
fn custom_login_targets_only_chat_completions_profiles() {
    let management = ManagementSnapshot::new(
        1,
        Vec::new(),
        vec![
            ProviderSummaryWire {
                provider: "custom-llama".to_owned(),
                api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
                endpoint: Some("http://127.0.0.1:18123/v1".to_owned()),
                models: vec!["llama3.1:8b".to_owned()],
                model_details: Vec::new(),
                auth_methods: Vec::new(),
                availability: haider_rpc::ProviderAvailabilityWire::Available,
                availability_reason: None,
                default_model: Some("llama3.1:8b".to_owned()),
                enabled: true,
            },
            ProviderSummaryWire {
                provider: "openai".to_owned(),
                api_family: ProviderApiFamilyWire::OpenAiResponses,
                endpoint: Some("https://api.openai.com/v1/responses".to_owned()),
                models: Vec::new(),
                model_details: Vec::new(),
                auth_methods: Vec::new(),
                availability: haider_rpc::ProviderAvailabilityWire::Available,
                availability_reason: None,
                default_model: None,
                enabled: true,
            },
            ProviderSummaryWire {
                provider: DEEPSEEK_PROVIDER_NAME.to_owned(),
                api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
                endpoint: Some(DEEPSEEK_BASE_URL.to_owned()),
                models: haider_provider::DEEPSEEK_SEED_MODELS
                    .iter()
                    .map(|model| (*model).to_owned())
                    .collect(),
                model_details: Vec::new(),
                auth_methods: vec![AuthMethod::ApiKey],
                availability: haider_rpc::ProviderAvailabilityWire::Unavailable,
                availability_reason: Some("provider has no credential".to_owned()),
                default_model: Some(haider_provider::DEEPSEEK_SEED_MODELS[0].to_owned()),
                enabled: true,
            },
        ],
    );
    let target = custom_login_target(Some(&management), "custom-llama")
        .expect("chat-completions profile is a login target");
    assert_eq!(target.0, "http://127.0.0.1:18123/v1");
    assert_eq!(target.1.as_deref(), Some("llama3.1:8b"));
    assert!(
        custom_login_target(Some(&management), "openai").is_none(),
        "a vendor-family profile NEVER validates against a stored origin"
    );
    assert!(
        custom_login_target(Some(&management), DEEPSEEK_PROVIDER_NAME).is_none(),
        "the named DeepSeek builtin never routes through custom validation"
    );
    assert!(custom_login_target(None, "custom-llama").is_none());
}

// ───────────────────── D1 device candidate import laws ──────────────────────

const KIMI_IMPORT_FIXTURE: &[u8] = br#"{
  "access_token": "fake-kimi-access-token-1",
  "refresh_token": "fake-kimi-refresh-token-1",
  "expires_at": 4102444800.0,
  "expires_in": 3600,
  "scope": "all",
  "token_type": "Bearer"
}"#;

const KIMI_DEVICE_ID_IMPORT_FIXTURE: &[u8] = b"6f2a9c31-77d4-4b8e-9a10-3c5de88f01ab";

const GEMINI_DISCOVERY_FIXTURE: &[u8] = br#"{
  "access_token": "fake-gemini-access-token-1",
  "refresh_token": "fake-gemini-refresh-token-1",
  "expiry_date": 4102444800123
}"#;

async fn send_import_device(
    commands: &mpsc::Sender<AccountCommand>,
    sink: Arc<dyn FrameSink>,
    command_id: &str,
    candidate: &str,
) {
    commands
        .send(AccountCommand::ImportDevice(Box::new(DeviceImportJob {
            command_id: command_id.to_owned(),
            candidate: candidate.to_owned(),
            discovery_disabled: false,
            route: LoginRoute {
                request_id: RequestId::new(format!("{command_id}-request")),
                sink,
            },
        })))
        .await
        .expect("send device import");
}

async fn expect_import_device_error(
    frames: &mut mpsc::UnboundedReceiver<WireFrame>,
    expected_message: &str,
) {
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("device import error deadline")
        .expect("device import error response");
    match frame {
        WireFrame::Response {
            body:
                ResponseBody::Error {
                    code,
                    message,
                    retryable,
                    ..
                },
            ..
        } => {
            assert_eq!(code, ERROR_CODE_INVALID_ARGUMENT);
            assert_eq!(message, expected_message);
            assert!(!retryable);
        }
        other => panic!("unexpected device import response: {other:?}"),
    }
}

/// LAW: import_device_is_receipted_and_lands_a_working_account. Codex and
/// kimi-code fixture stores travel the real machinery end to end — discovery
/// mints the opaque candidate, `account.import_device` re-discovers and
/// routes through the per-source import parsers — and each import answers
/// with its own response method, lands a resolvable vault bundle (plus the
/// kimi first-party device identity), and writes a durable receipt that
/// names the candidate and carries no token bytes.
#[tokio::test(flavor = "current_thread")]
async fn import_device_is_receipted_and_lands_a_working_account() {
    let fixture_dir = test_store_dir();
    let codex_path = fixture_dir.path().join("codex-auth.json");
    std::fs::write(&codex_path, CODEX_IMPORT_FIXTURE_1).expect("write Codex fixture");
    let kimi_path = fixture_dir.path().join("kimi-code.json");
    std::fs::write(&kimi_path, KIMI_IMPORT_FIXTURE).expect("write Kimi fixture");
    let kimi_device_path = fixture_dir.path().join("kimi-device-id");
    std::fs::write(&kimi_device_path, KIMI_DEVICE_ID_IMPORT_FIXTURE).expect("write Kimi device id");
    let empty_home = fixture_dir.path().join("empty-home");
    std::fs::create_dir_all(&empty_home).expect("mkdir empty home");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::import_device_is_receipted_and_lands_a_working_account",
        &[
            ("HAIDER_CODEX_AUTH_PATH", &codex_path),
            ("HAIDER_KIMI_CREDS_PATH", &kimi_path),
            ("HAIDER_KIMI_DEVICE_ID_PATH", &kimi_device_path),
            ("HOME", &empty_home),
        ],
    ) {
        return;
    }

    let candidates = crate::device_discovery::discover_device_candidates(false);
    let candidate_for = |provider: &str| {
        candidates
            .iter()
            .find(|candidate| candidate.wire.provider == provider)
            .unwrap_or_else(|| panic!("missing {provider} candidate in {candidates:?}"))
    };
    let codex_candidate = candidate_for(OPENAI_OAUTH_PROVIDER_NAME);
    assert!(codex_candidate.wire.import_supported);
    let codex_candidate = codex_candidate.wire.candidate.clone();
    let kimi_candidate = candidate_for(KIMI_OAUTH_PROVIDER_NAME);
    assert!(kimi_candidate.wire.import_supported);
    let kimi_candidate = kimi_candidate.wire.candidate.clone();

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, snapshot, management) = start_oauth_import_test_actor(
        &store,
        Arc::clone(&vault),
        HashSet::new(),
        RefreshFenceRegistry::default(),
    );
    let (sink, mut frames) = channel_sink();

    send_import_device(
        &actor.commands(),
        Arc::clone(&sink),
        "import-device-codex-1",
        &codex_candidate,
    )
    .await;
    let codex_descriptor = match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("Codex device import deadline")
        .expect("Codex device import response")
    {
        WireFrame::Response {
            body:
                ResponseBody::AccountImportDevice {
                    descriptor,
                    revision: 1,
                },
            ..
        } => descriptor,
        other => panic!("unexpected Codex device import response: {other:?}"),
    };
    assert_eq!(codex_descriptor.alias.as_str(), OPENAI_OAUTH_PROVIDER_NAME);
    assert_eq!(codex_descriptor.provider, OPENAI_OAUTH_PROVIDER_NAME);
    assert_eq!(codex_descriptor.identity, "fake-account-1");
    assert!(codex_descriptor.active);
    let stored = vault
        .resolve(&CredentialAlias::new(OPENAI_OAUTH_PROVIDER_NAME))
        .expect("stored Codex bundle");
    let bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode Codex bundle");
    assert_eq!(bundle.provider_id, OPENAI_OAUTH_PROVIDER_NAME);
    assert_eq!(bundle.access_token(), b"fake-access-token-1");
    assert_eq!(
        bundle.refresh_token(),
        Some(b"fake-refresh-token-1".as_slice())
    );
    assert_eq!(bundle.generation, 1);

    send_import_device(
        &actor.commands(),
        Arc::clone(&sink),
        "import-device-kimi-1",
        &kimi_candidate,
    )
    .await;
    let kimi_descriptor = match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("Kimi device import deadline")
        .expect("Kimi device import response")
    {
        WireFrame::Response {
            body:
                ResponseBody::AccountImportDevice {
                    descriptor,
                    revision: 2,
                },
            ..
        } => descriptor,
        other => panic!("unexpected Kimi device import response: {other:?}"),
    };
    assert_eq!(kimi_descriptor.alias.as_str(), KIMI_OAUTH_PROVIDER_NAME);
    assert_eq!(kimi_descriptor.provider, KIMI_OAUTH_PROVIDER_NAME);
    assert_eq!(kimi_descriptor.identity, "Kimi Code subscription");
    assert!(kimi_descriptor.active);
    let stored = vault
        .resolve(&CredentialAlias::new(KIMI_OAUTH_PROVIDER_NAME))
        .expect("stored Kimi bundle");
    let kimi_bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
        .expect("decode Kimi bundle");
    assert_eq!(kimi_bundle.provider_id, KIMI_OAUTH_PROVIDER_NAME);
    assert_eq!(kimi_bundle.access_token(), b"fake-kimi-access-token-1");
    assert_eq!(
        kimi_bundle.refresh_token(),
        Some(b"fake-kimi-refresh-token-1".as_slice())
    );
    assert_eq!(kimi_bundle.expires_at_unix_ms, 4_102_444_800_000);
    assert_eq!(kimi_bundle.granted_scopes, ["all"]);
    let device_identity = vault
        .resolve(&CredentialAlias::new(KIMI_DEVICE_ALIAS))
        .expect("stored Kimi device identity");
    assert_eq!(
        device_identity.expose_secret(),
        KIMI_DEVICE_ID_IMPORT_FIXTURE
    );

    assert_eq!(snapshot.lock().expect("snapshot").len(), 2);
    assert_eq!(management.read().expect("management").revision, 2);

    let receipts = store.account_add_receipts().await.expect("import receipts");
    let receipt_for = |command_id: &str| {
        receipts
            .iter()
            .find(|row| row.command_id == command_id)
            .unwrap_or_else(|| panic!("missing durable receipt for {command_id}"))
    };
    let codex_receipt = receipt_for("import-device-codex-1");
    assert_eq!(
        codex_receipt.request_json,
        format!(
            r#"{{"source":"codex","alias":"openai-oauth","provider":"openai-oauth","candidate":"{codex_candidate}"}}"#
        )
    );
    let kimi_receipt = receipt_for("import-device-kimi-1");
    assert_eq!(
        kimi_receipt.request_json,
        format!(
            r#"{{"source":"kimi-code","alias":"kimi-oauth","provider":"kimi-oauth","candidate":"{kimi_candidate}"}}"#
        )
    );
    for receipt in [codex_receipt, kimi_receipt] {
        let durable = format!(
            "{}{}",
            receipt.request_json,
            receipt.response_json.as_deref().unwrap_or_default()
        );
        for secret in [
            "fake-access-token-1",
            "fake-refresh-token-1",
            "fake-id-token-1",
            "fake-kimi-access-token-1",
            "fake-kimi-refresh-token-1",
            "6f2a9c31-77d4-4b8e-9a10-3c5de88f01ab",
        ] {
            assert!(!durable.contains(secret), "receipt leaked {secret}");
        }
    }

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// LAW: unsupported_candidate_is_honest_not_guessed. A discoverable store
/// without a sanctioned import parser (gemini), or missing its required
/// first-party companion material (kimi-code without a device identity), is
/// reported with `import_supported:false` and an honest reason — and
/// `account.import_device` refuses with that same reason instead of guessing
/// a parser. Nothing is committed, receipted, or stored.
#[tokio::test(flavor = "current_thread")]
async fn unsupported_candidate_is_honest_not_guessed() {
    let fixture_dir = test_store_dir();
    let gemini_path = fixture_dir.path().join("gemini-oauth-creds.json");
    std::fs::write(&gemini_path, GEMINI_DISCOVERY_FIXTURE).expect("write Gemini fixture");
    let kimi_path = fixture_dir.path().join("kimi-code.json");
    std::fs::write(&kimi_path, KIMI_IMPORT_FIXTURE).expect("write Kimi fixture");
    let kimi_device_path = fixture_dir.path().join("missing-kimi-device-id");
    let empty_home = fixture_dir.path().join("empty-home");
    std::fs::create_dir_all(&empty_home).expect("mkdir empty home");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::unsupported_candidate_is_honest_not_guessed",
        &[
            ("HAIDER_GEMINI_CREDS_PATH", &gemini_path),
            ("HAIDER_KIMI_CREDS_PATH", &kimi_path),
            ("HAIDER_KIMI_DEVICE_ID_PATH", &kimi_device_path),
            ("HOME", &empty_home),
        ],
    ) {
        return;
    }

    let candidates = crate::device_discovery::discover_device_candidates(false);
    let candidate_for = |provider: &str| {
        candidates
            .iter()
            .find(|candidate| candidate.wire.provider == provider)
            .unwrap_or_else(|| panic!("missing {provider} candidate in {candidates:?}"))
    };
    let gemini = candidate_for("gemini");
    assert!(!gemini.wire.import_supported);
    assert!(gemini.import_source.is_none());
    let gemini_reason = gemini
        .wire
        .unsupported_reason
        .clone()
        .expect("gemini honest reason");
    assert!(
        gemini_reason.contains("cannot be imported"),
        "honest reason: {gemini_reason}"
    );
    let kimi = candidate_for(KIMI_OAUTH_PROVIDER_NAME);
    assert!(!kimi.wire.import_supported);
    assert!(kimi.import_source.is_none());
    let kimi_reason = kimi
        .wire
        .unsupported_reason
        .clone()
        .expect("kimi honest reason");
    assert!(
        kimi_reason.contains("device identity"),
        "honest reason: {kimi_reason}"
    );

    let store_dir = test_store_dir();
    let store = open_store(store_dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let (mut actor, snapshot, management) = start_oauth_import_test_actor(
        &store,
        Arc::clone(&vault),
        HashSet::new(),
        RefreshFenceRegistry::default(),
    );
    let (sink, mut frames) = channel_sink();

    send_import_device(
        &actor.commands(),
        Arc::clone(&sink),
        "import-device-gemini-1",
        &gemini.wire.candidate,
    )
    .await;
    expect_import_device_error(&mut frames, &gemini_reason).await;

    send_import_device(
        &actor.commands(),
        Arc::clone(&sink),
        "import-device-kimi-unsupported-1",
        &kimi.wire.candidate,
    )
    .await;
    expect_import_device_error(&mut frames, &kimi_reason).await;

    // A well-formed but unknown candidate id is honestly unavailable, never
    // resolved to a guessed path or parser.
    let unknown = format!("dc1_{}", "f".repeat(64));
    send_import_device(
        &actor.commands(),
        Arc::clone(&sink),
        "import-device-unknown-1",
        &unknown,
    )
    .await;
    expect_import_device_error(&mut frames, "device credential candidate is unavailable").await;

    assert!(snapshot.lock().expect("snapshot").is_empty());
    assert_eq!(management.read().expect("management").revision, 0);
    assert!(
        store
            .account_add_receipts()
            .await
            .expect("import receipts")
            .is_empty(),
        "a refused candidate must never leave a durable receipt"
    );
    assert!(
        vault
            .resolve(&CredentialAlias::new(KIMI_OAUTH_PROVIDER_NAME))
            .is_err(),
        "a refused candidate must never land a vault bundle"
    );

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// LAW (LE4, construction-gate half): the per-turn tuning derived from
/// session metadata carries effort/fast verbatim, and the CONSTRUCTION fast
/// gate filters a stale flag on any pair outside the static table — a model
/// switch after `/fast on` must yield standard requests, never the
/// documented 4.7 hard error or 4.6 silent-standard billing.
#[test]
fn provider_tuning_derives_from_metadata_and_fast_gate_filters_stale_pairs() {
    use crate::accounts::{ProviderTuning, anthropic_fast_for};

    let metadata = haider_protocol::session::SessionMetadataV1 {
        cwd: "/tmp".into(),
        provider: "anthropic-oauth".into(),
        model: "claude-opus-5".into(),
        max_tokens: 4096,
        system_prompt_version: None,
        permission_overrides: None,
        title: None,
        effort: Some("xhigh".into()),
        fast: true,
        cache_policy: Default::default(),
        created_at_ms: 1,
    };
    let tuning = ProviderTuning::from_metadata(&metadata);
    assert_eq!(tuning.effort.as_deref(), Some("xhigh"));
    assert!(tuning.fast);

    assert!(anthropic_fast_for(
        "anthropic-oauth",
        &tuning,
        "claude-opus-5"
    ));
    assert!(anthropic_fast_for("anthropic", &tuning, "claude-opus-4-8"));
    for stale in ["claude-opus-4-7", "claude-opus-4-6", "claude-sonnet-5"] {
        assert!(
            !anthropic_fast_for("anthropic", &tuning, stale),
            "a stale fast flag on {stale} must not reach the wire"
        );
    }
    // G4b (LE-x construction half): bedrock/vertex refuse fast REGARDLESS
    // of model — the normalized gate would otherwise admit the enterprise
    // spellings of opus-5 — while the SAME pairs stay admitted on the
    // first-party providers (both directions).
    for enterprise in ["bedrock", "vertex"] {
        for model in [
            "anthropic.claude-opus-5",
            "claude-opus-5",
            "claude-opus-4-8@20260115",
        ] {
            assert!(
                !anthropic_fast_for(enterprise, &tuning, model),
                "fast must refuse on {enterprise} for {model}"
            );
        }
    }
    assert!(anthropic_fast_for(
        "anthropic",
        &tuning,
        "anthropic.claude-opus-5"
    ));
    let off = ProviderTuning {
        effort: None,
        fast: false,
        web_tools: false,
    };
    assert!(!anthropic_fast_for("anthropic", &off, "claude-opus-5"));
}

/// LAW (review of record, construction-gate effort half): a stale effort
/// after a model switch NEVER rides onto a wire the pair documents as
/// invalid. Anthropic clamps down the documented ladder (Claude Code's
/// fallback rule: `xhigh` on a 4.6 row -> `high`); a ladder-known pair with
/// an out-of-vocabulary value drops to provider default; only ladder-unknown
/// models pass the selection through verbatim. OpenAI pairs source the gate
/// from the pair's CATALOG ladder: declared-and-excluded drops to `None`,
/// declared-empty passes verbatim (vocabularies differ across families — no
/// invented cross-family fallback).
#[test]
fn stale_effort_clamps_for_anthropic_and_drops_for_declared_openai_ladders() {
    use crate::accounts::{ProviderTuning, anthropic_effort_for, openai_effort_for};

    let tuning = ProviderTuning {
        effort: Some("xhigh".to_owned()),
        fast: false,
        web_tools: false,
    };
    assert_eq!(
        anthropic_effort_for(&tuning, "claude-fable-5").as_deref(),
        Some("xhigh"),
        "a supported level passes untouched"
    );
    assert_eq!(
        anthropic_effort_for(&tuning, "claude-opus-4-6").as_deref(),
        Some("high"),
        "xhigh clamps DOWN to high on the max-not-xhigh ladder"
    );
    assert_eq!(
        anthropic_effort_for(&tuning, "claude-mystery-9").as_deref(),
        Some("xhigh"),
        "an unknown model passes the selection through verbatim"
    );
    let garbage = ProviderTuning {
        effort: Some("turbo".to_owned()),
        fast: false,
        web_tools: false,
    };
    assert_eq!(
        anthropic_effort_for(&garbage, "claude-opus-5"),
        None,
        "an out-of-vocabulary value on a known ladder drops to provider default"
    );

    let summary = ProviderSummaryWire {
        provider: "openai-oauth".to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiResponses,
        endpoint: None,
        models: vec!["gpt-5.5".to_owned()],
        model_details: vec![
            ModelDetailWire {
                name: "gpt-5.5".to_owned(),
                context_window: Some(400_000),
                supported_efforts: vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
                default_effort: Some("medium".to_owned()),
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
            },
            ModelDetailWire {
                name: "gpt-5.6-sol".to_owned(),
                context_window: Some(400_000),
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
            },
        ],
        auth_methods: vec![AuthMethod::OAuth],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("gpt-5.5".to_owned()),
        enabled: true,
    };
    assert_eq!(
        openai_effort_for(&tuning, Some(&summary), "gpt-5.5"),
        None,
        "a declared ladder that excludes the stale level drops it"
    );
    let supported = ProviderTuning {
        effort: Some("medium".to_owned()),
        fast: false,
        web_tools: false,
    };
    assert_eq!(
        openai_effort_for(&supported, Some(&summary), "gpt-5.5").as_deref(),
        Some("medium"),
        "a declared ladder that includes the level passes it"
    );
    assert_eq!(
        openai_effort_for(&tuning, Some(&summary), "gpt-5.6-sol").as_deref(),
        Some("xhigh"),
        "a declared-EMPTY ladder passes the selection through verbatim"
    );
    assert_eq!(
        openai_effort_for(&tuning, None, "gpt-5.5").as_deref(),
        Some("xhigh"),
        "no profile at all passes the selection through verbatim"
    );
}

// ───────────────────────────── G4b enterprise laws ──────────────────────────

fn enterprise_summary(provider: &str, endpoint: Option<&str>) -> ProviderSummaryWire {
    let (models, default_model): (Vec<String>, &str) = if provider == "bedrock" {
        (
            haider_provider::BEDROCK_SEED_MODELS
                .iter()
                .map(|slug| (*slug).to_owned())
                .collect(),
            "anthropic.claude-fable-5",
        )
    } else {
        (
            haider_provider::VERTEX_SEED_MODELS
                .iter()
                .map(|slug| (*slug).to_owned())
                .collect(),
            "claude-fable-5",
        )
    };
    ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: ProviderApiFamilyWire::AnthropicMessages,
        endpoint: endpoint.map(str::to_owned),
        models: models.clone(),
        model_details: Vec::new(),
        auth_methods: vec![AuthMethod::ApiKey],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some(default_model.to_owned()),
        enabled: true,
    }
}

fn enterprise_metadata(provider: &str, model: &str) -> haider_protocol::session::SessionMetadataV1 {
    haider_protocol::session::SessionMetadataV1 {
        cwd: "/tmp/enterprise-dispatch".to_owned(),
        provider: provider.to_owned(),
        model: model.to_owned(),
        max_tokens: 64,
        permission_overrides: None,
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        created_at_ms: 1,
    }
}

/// LAW (G4b factory arms + decision 5 surfaces): a stored key resolves the
/// bedrock pair through the mantle-pinned Anthropic adapter (ApiKey surface
/// — the EXACT x-api-key reuse) and the vertex pair through the vertex
/// adapter (the new CloudBearer surface); a vertex profile with NO endpoint
/// refuses construction with a typed reason instead of guessing a URL.
///
/// MUTATION CHECK: delete the `(BEDROCK_PROVIDER_NAME, ApiKey)` (or vertex)
/// arm from `build_account_provider`. Expected RUNTIME failure: resolution
/// reports "no account-backed adapter" instead of the surfaces below.
#[tokio::test]
async fn g4b_factory_builds_bedrock_and_vertex_adapters_with_their_surfaces() {
    let dispatch = |provider: &str, endpoint: Option<&str>, model: &str| {
        let summary = enterprise_summary(provider, endpoint);
        let alias = CredentialAlias::new(format!("{provider}-key"));
        let vault = Arc::new(MemoryVault::default());
        vault
            .put(&alias, b"ENTERPRISE_BEARER_SENTINEL_1f2e")
            .unwrap_or_else(|error| panic!("seed key: {error:?}"));
        let descriptor = CredentialDescriptor {
            alias: alias.clone(),
            provider: provider.to_owned(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: format!("{provider} bearer"),
            status: CredentialStatus::Ok,
            active: true,
        };
        let snapshot = Arc::new(StdMutex::new(vec![descriptor.clone()]));
        let management = ManagementSnapshot::new(0, vec![descriptor], vec![summary]);
        let factory = AccountsProviderFactory::new_with_management(
            snapshot,
            management,
            VaultProvision::Available(vault as Arc<dyn Vault>),
            Arc::new(ProductionAccountBuilder),
        );
        let metadata = enterprise_metadata(provider, model);
        async move { factory.resolve_for_turn(&metadata).await }
    };

    let bedrock = dispatch(
        "bedrock",
        Some("https://bedrock-mantle.us-east-1.api.aws/anthropic"),
        "anthropic.claude-fable-5",
    )
    .await
    .expect("bedrock dispatch");
    assert_eq!(bedrock.provider_name, "bedrock");
    assert_eq!(
        bedrock.provider.credential_surface(),
        haider_provider::ProviderCredentialSurface::ApiKey,
        "the mantle bearer is the exact x-api-key surface (decision 5)"
    );

    let vertex = dispatch(
        "vertex",
        Some(
            "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models",
        ),
        "claude-fable-5",
    )
    .await
    .expect("vertex dispatch");
    assert_eq!(
        vertex.provider.credential_surface(),
        haider_provider::ProviderCredentialSurface::CloudBearer,
        "the vertex adapter reports the new CloudBearer surface (decision 5)"
    );

    let endpoint_less = match dispatch("vertex", None, "claude-fable-5").await {
        Err(error) => error,
        Ok(_) => panic!("an endpoint-less vertex profile must refuse construction"),
    };
    assert!(
        endpoint_less.message.contains("project endpoint"),
        "the refusal names the missing endpoint: {}",
        endpoint_less.message
    );
}

/// A scripted gcloud source for the LV2 laws.
#[derive(Default)]
struct ScriptedGcloud {
    responses: StdMutex<std::collections::VecDeque<Result<Vec<u8>, String>>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl ScriptedGcloud {
    fn push_token(&self, token: &[u8]) {
        if let Ok(mut responses) = self.responses.lock() {
            responses.push_back(Ok(token.to_vec()));
        }
    }

    fn push_failure(&self, reason: &str) {
        if let Ok(mut responses) = self.responses.lock() {
            responses.push_back(Err(reason.to_owned()));
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl crate::gcloud::GcloudAccessTokenSource for ScriptedGcloud {
    fn print_access_token(&self) -> Result<zeroize::Zeroizing<Vec<u8>>, HaiderError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self
            .responses
            .lock()
            .ok()
            .and_then(|mut responses| responses.pop_front())
        {
            Some(Ok(token)) => Ok(zeroize::Zeroizing::new(token)),
            Some(Err(reason)) => Err(crate::gcloud::gcloud_error(reason)),
            None => Err(crate::gcloud::gcloud_error("script exhausted")),
        }
    }
}

/// LAW (LV2 — the gcloud refresh source): an auth failure on THE vertex
/// gcloud descriptor re-mints the token through the mocked shell-out and
/// PERSISTS it in the vault before the retry; a failing shell-out surfaces
/// the typed gcloud error (vault untouched); and every other ApiKey
/// descriptor keeps the classic non-refreshable auth failure with the
/// source never invoked (both directions).
///
/// MUTATION CHECK: drop the `vault.put` from `refresh_gcloud` (return the
/// fresh token without persisting). Expected RUNTIME failure: the
/// vault-persistence equality below.
#[tokio::test]
async fn lv2_gcloud_refresh_source_refreshes_vault_and_surfaces_failure() {
    let vault = Arc::new(MemoryVault::default());
    let alias = CredentialAlias::new(crate::gcloud::VERTEX_GCLOUD_ALIAS);
    vault
        .put(&alias, b"STALE_GCLOUD_TOKEN_00aa")
        .expect("seed stale token");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let (status_commands, _status_receiver) = mpsc::channel(4);
    let gcloud = Arc::new(ScriptedGcloud::default());
    gcloud.push_token(b"FRESH_GCLOUD_TOKEN_11bb");
    gcloud.push_failure("gcloud exited with exit status: 1: Reauthentication required");
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::default(),
        Arc::clone(&snapshot),
        status_commands,
    )
    .expect("credential broker")
    .with_gcloud_source(gcloud.clone() as Arc<dyn crate::gcloud::GcloudAccessTokenSource>)
    .expect("install scripted source before sharing");

    let descriptor = CredentialDescriptor {
        alias: alias.clone(),
        provider: "vertex".to_owned(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "gcloud access token (auto-refresh)".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let refreshed = broker
        .refresh_after_auth_failure(&descriptor, None)
        .await
        .expect("gcloud refresh");
    assert_eq!(refreshed.expose_secret(), b"FRESH_GCLOUD_TOKEN_11bb");
    assert_eq!(
        vault
            .resolve(&alias)
            .expect("refreshed vault entry")
            .expose_secret(),
        b"FRESH_GCLOUD_TOKEN_11bb",
        "the refresh PERSISTS in the vault, not just the returned handle"
    );
    assert_eq!(gcloud.calls(), 1);

    // A failing shell-out surfaces honestly and leaves the vault alone.
    let failure = broker
        .refresh_after_auth_failure(&descriptor, None)
        .await
        .expect_err("scripted failure surfaces");
    assert!(
        failure.message.contains("gcloud"),
        "the error names gcloud: {}",
        failure.message
    );
    assert_eq!(
        vault.resolve(&alias).expect("vault entry").expose_secret(),
        b"FRESH_GCLOUD_TOKEN_11bb"
    );

    // Every OTHER ApiKey descriptor keeps the classic refusal — and never
    // touches the shell-out source.
    let plain = CredentialDescriptor {
        alias: CredentialAlias::new("vertex-key"),
        provider: "vertex".to_owned(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "pasted access token".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let refused = broker
        .refresh_after_auth_failure(&plain, None)
        .await
        .expect_err("plain keys stay non-refreshable");
    assert!(refused.message.contains("authentication failed"));
    assert_eq!(gcloud.calls(), 2, "the plain-key refusal never shells out");
}

/// LAW (LA-x, env-bridge half): with `AWS_BEARER_TOKEN_BEDROCK` set and no
/// bedrock descriptor, startup imports the value through the accounts env
/// bridge — deterministic `bedrock-env` alias, active ApiKey descriptor,
/// token in the vault — and an EXISTING bedrock descriptor suppresses the
/// import entirely (an explicit login is never fought). Child-process
/// isolated: the variable is process-global.
///
/// MUTATION CHECK: drop the `import_bedrock_env_bearer` call from
/// `AccountsRuntime::initialize` (the unit here drives the same fn the
/// initialize path calls; the descriptor/vault assertions fail if the
/// import stops importing).
#[tokio::test]
async fn la_env_bridge_imports_aws_bearer_token_bedrock() {
    let token_value = std::path::Path::new("BEDROCK_ENV_SENTINEL_9c3d");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::la_env_bridge_imports_aws_bearer_token_bedrock",
        &[(crate::accounts::BEDROCK_ENV_BEARER_VAR, token_value)],
    ) {
        return;
    }
    let vault: Arc<dyn Vault> = Arc::new(MemoryVault::default());
    let provision = VaultProvision::Available(Arc::clone(&vault));
    let mut accounts = memory_accounts();
    import_bedrock_env_bearer(&mut accounts, &provision);
    let descriptor = accounts
        .list()
        .iter()
        .find(|descriptor| descriptor.provider == "bedrock")
        .cloned()
        .expect("env bridge imported a bedrock descriptor");
    assert_eq!(descriptor.alias.as_str(), "bedrock-env");
    assert_eq!(descriptor.auth_method, AuthMethod::ApiKey);
    assert!(descriptor.active);
    assert_eq!(
        vault
            .resolve(&descriptor.alias)
            .expect("vaulted env token")
            .expose_secret(),
        b"BEDROCK_ENV_SENTINEL_9c3d"
    );

    // Idempotence + suppression: a second boot with the descriptor present
    // imports NOTHING (the existing account is never fought).
    import_bedrock_env_bearer(&mut accounts, &provision);
    assert_eq!(
        accounts
            .list()
            .iter()
            .filter(|descriptor| descriptor.provider == "bedrock")
            .count(),
        1
    );
}

/// LAW (G4b login target): `account.login_api` for an enterprise builtin
/// validates through the injected validator AT the profile's stored
/// endpoint WITH the profile's declared default-model spelling — never the
/// global vendor default — and commits the descriptor under the provider.
///
/// MUTATION CHECK: drop the `enterprise_login_target` endpoint from the
/// validator call in `handle_login`. Expected RUNTIME failure: the recorded
/// endpoint below is `None`.
#[tokio::test]
async fn enterprise_login_validates_at_the_profile_endpoint_with_its_default_model() {
    #[derive(Default)]
    struct RecordingValidator {
        seen: StdMutex<Vec<(String, String, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl CredentialValidator for RecordingValidator {
        fn supports(&self, provider: &str) -> bool {
            provider == "bedrock"
        }

        async fn validate(
            &self,
            provider: &str,
            model: &str,
            _secret: &[u8],
            endpoint: Option<&str>,
        ) -> Result<ValidatedIdentity, ValidationError> {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push((
                    provider.to_owned(),
                    model.to_owned(),
                    endpoint.map(str::to_owned),
                ));
            }
            Ok(ValidatedIdentity {
                identity: "bedrock bearer".into(),
            })
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let mut accounts = memory_accounts();
    let vault = MemoryVault::default();
    let validator = RecordingValidator::default();
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let management = ManagementSnapshot::new(
        0,
        Vec::new(),
        vec![enterprise_summary(
            "bedrock",
            Some("https://bedrock-mantle.us-east-1.api.aws/anthropic"),
        )],
    );
    let mut pending: HashMap<String, PendingSecret> = HashMap::new();
    let (sink, mut frames) = channel_sink();
    handle_login(
        &store,
        &mut accounts,
        &vault,
        &validator,
        &snapshot,
        Some(&management),
        &login_registry(),
        "profile-bedrock",
        "claude-global-default",
        &mut pending,
        &HashSet::new(),
        LoginJob {
            command_id: "bedrock-login".to_owned(),
            provider: "bedrock".to_owned(),
            display_alias: Some("bedrock-main".to_owned()),
            validation_model: None,
            secret: Some(Zeroizing::new(b"BEDROCK_LOGIN_SENTINEL".to_vec())),
            route: LoginRoute {
                request_id: RequestId::new("bedrock-login-request"),
                sink: Arc::clone(&sink),
            },
        },
    )
    .await;
    let frame = frames.try_recv().expect("login response");
    match frame {
        WireFrame::Response {
            body: ResponseBody::AccountLoginApi { descriptor },
            ..
        } => {
            assert_eq!(descriptor.provider, "bedrock");
            assert_eq!(descriptor.alias.as_str(), "bedrock-main");
        }
        other => panic!("expected a committed login, got {other:?}"),
    }
    let seen = validator.seen.lock().expect("recorded validations").clone();
    assert_eq!(
        seen,
        vec![(
            "bedrock".to_owned(),
            "anthropic.claude-fable-5".to_owned(),
            Some("https://bedrock-mantle.us-east-1.api.aws/anthropic".to_owned()),
        )],
        "validation runs at the profile endpoint with the profile default model"
    );
    store.close().await.expect("close store");
}

/// LAW (G4b factory route): azure-origin custom profiles route through the
/// `api-key` adapter, non-azure customs keep the TrustedLan custom route,
/// and builtin ids keep the strict Bearer route — the ONE mapping behind
/// `custom_compatible_adapter`, both directions.
#[test]
fn azure_origin_custom_profiles_route_through_the_api_key_header_adapter() {
    use crate::accounts::{CompatibleAdapterRoute, compatible_adapter_route};
    assert_eq!(
        compatible_adapter_route("azure", "https://contoso.openai.azure.com/openai/v1"),
        CompatibleAdapterRoute::Azure
    );
    assert_eq!(
        compatible_adapter_route(
            "my-azure-lane",
            "https://acme.services.ai.azure.com/openai/v1"
        ),
        CompatibleAdapterRoute::Azure
    );
    assert_eq!(
        compatible_adapter_route("vllm-lab", "http://127.0.0.1:8000/v1"),
        CompatibleAdapterRoute::Custom
    );
    assert_eq!(
        compatible_adapter_route("openai-compatible", "https://gateway.example.com/v1"),
        CompatibleAdapterRoute::Builtin
    );
    assert_eq!(
        compatible_adapter_route(
            "vllm-lab",
            "https://contoso.openai.azure.com.evil.example/v1"
        ),
        CompatibleAdapterRoute::Custom,
        "a lookalike host never inherits the azure header mode"
    );
}

/// LAW (LV2, import half — the D-wave pattern): the gcloud ADC discovery
/// candidate imports by RUNNING the (mocked) shell-out — never by copying a
/// credential file — vaults the token under the fixed `vertex-gcloud`
/// alias, commits an active vertex descriptor, and publishes the FULL
/// provider view so a configured vertex profile lights Available in the
/// same stroke; a re-import refreshes the token in place (no duplicate).
/// Child-process isolated: discovery reads the config-dir env override.
///
/// MUTATION CHECK: route the gcloud source through `handle_oauth_import`
/// (drop the dedicated arm). Expected RUNTIME failure: the import errors on
/// the bundle machinery instead of committing the descriptor below.
#[tokio::test]
async fn lv2_gcloud_device_import_vaults_the_token_and_lights_vertex() {
    let fixture_dir = test_store_dir();
    std::fs::write(
        fixture_dir
            .path()
            .join("application_default_credentials.json"),
        b"{\"type\":\"authorized_user\"}",
    )
    .expect("write ADC marker");
    if run_oauth_import_env_child(
        "accounts::accounts_tests::lv2_gcloud_device_import_vaults_the_token_and_lights_vertex",
        &[("HAIDER_GCLOUD_CONFIG_DIR", fixture_dir.path())],
    ) {
        return;
    }

    let candidate = crate::device_discovery::discover_device_candidates(false)
        .into_iter()
        .find(|candidate| candidate.wire.provider == "vertex")
        .expect("gcloud ADC candidate discovered");
    assert!(candidate.wire.import_supported);
    assert_eq!(candidate.wire.source_label, "Google Cloud (gcloud ADC)");

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let mut accounts = memory_accounts();
    let vault: Arc<dyn Vault> = Arc::new(MemoryVault::default());
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    // A vertex profile whose card already supplied the endpoint: the
    // import is exactly what flips it Available. The registry seeds the
    // builtin vertex profile exactly like daemon initialize does.
    let provider_store: Box<dyn ProviderRegistryStoreLike> = Box::new(TestProviderStore::default());
    let mut providers = ProviderRegistry::new(
        provider_store,
        crate::provider_registry::initial_provider_profiles(
            &std::collections::BTreeSet::from(["vertex".to_owned()]),
            "unused",
        ),
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("seeded vertex registry");
    let vertex_models: Vec<String> = haider_provider::VERTEX_SEED_MODELS
        .iter()
        .map(|slug| (*slug).to_owned())
        .collect();
    providers
        .configure(crate::provider_registry::ProviderConfigureInput {
            provider: "vertex".to_owned(),
            api_family: Some(ProviderApiFamilyWire::AnthropicMessages),
            origin: Some(
                "https://aiplatform.googleapis.com/v1/projects/acme-ai/locations/global/publishers/anthropic/models"
                    .to_owned(),
            ),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vertex_models,
            default_model: Some("claude-fable-5".to_owned()),
        })
        .expect("configure vertex endpoint");
    let management = ManagementSnapshot::new(
        0,
        Vec::new(),
        providers.summaries(&provider_has_credential(&accounts)),
    );
    let before = management
        .read()
        .expect("pre-import view")
        .providers
        .into_iter()
        .find(|summary| summary.provider == "vertex")
        .expect("vertex summary");
    assert_eq!(
        before.availability,
        haider_rpc::ProviderAvailabilityWire::Unavailable,
        "no credential yet"
    );

    let gcloud = Arc::new(ScriptedGcloud::default());
    gcloud.push_token(b"GCLOUD_IMPORT_TOKEN_31ab");
    gcloud.push_token(b"GCLOUD_REIMPORT_TOKEN_42cd");
    let (sink, mut frames) = channel_sink();
    let job = |command_id: &str, request_id: &str| DeviceImportJob {
        command_id: command_id.to_owned(),
        candidate: candidate.wire.candidate.clone(),
        discovery_disabled: false,
        route: LoginRoute {
            request_id: RequestId::new(request_id),
            sink: Arc::clone(&sink),
        },
    };
    handle_device_import(
        &store,
        &mut accounts,
        Arc::clone(&vault),
        &snapshot,
        Some(&management),
        &providers,
        &HashSet::new(),
        &RefreshFenceRegistry::default(),
        gcloud.clone() as Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
        Arc::new(PlatformClaudeNativeCredentialStore::default()),
        job("gcloud-import-1", "req-1"),
    )
    .await;
    let alias = CredentialAlias::new(crate::gcloud::VERTEX_GCLOUD_ALIAS);
    match frames.try_recv().expect("import response") {
        WireFrame::Response {
            body: ResponseBody::AccountImportDevice { descriptor, .. },
            ..
        } => {
            assert_eq!(descriptor.provider, "vertex");
            assert_eq!(descriptor.alias, alias);
            assert!(descriptor.active);
            assert_eq!(descriptor.auth_method, AuthMethod::ApiKey);
        }
        other => panic!("expected a committed device import, got {other:?}"),
    }
    assert_eq!(
        vault
            .resolve(&alias)
            .expect("vaulted gcloud token")
            .expose_secret(),
        b"GCLOUD_IMPORT_TOKEN_31ab"
    );
    let after = management
        .read()
        .expect("post-import view")
        .providers
        .into_iter()
        .find(|summary| summary.provider == "vertex")
        .expect("vertex summary");
    assert_eq!(
        after.availability,
        haider_rpc::ProviderAvailabilityWire::Available,
        "the imported credential lights the configured vertex profile"
    );

    // Re-import = refresh: same alias, fresh token, still exactly one
    // vertex descriptor.
    handle_device_import(
        &store,
        &mut accounts,
        Arc::clone(&vault),
        &snapshot,
        Some(&management),
        &providers,
        &HashSet::new(),
        &RefreshFenceRegistry::default(),
        gcloud.clone() as Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
        Arc::new(PlatformClaudeNativeCredentialStore::default()),
        job("gcloud-import-2", "req-2"),
    )
    .await;
    match frames.try_recv().expect("re-import response") {
        WireFrame::Response {
            body: ResponseBody::AccountImportDevice { descriptor, .. },
            ..
        } => assert_eq!(descriptor.alias, alias),
        other => panic!("expected a committed re-import, got {other:?}"),
    }
    assert_eq!(
        vault
            .resolve(&alias)
            .expect("refreshed token")
            .expose_secret(),
        b"GCLOUD_REIMPORT_TOKEN_42cd"
    );
    assert_eq!(
        accounts
            .list()
            .iter()
            .filter(|descriptor| descriptor.provider == "vertex")
            .count(),
        1
    );
    store.close().await.expect("close store");
}

/// Records the [`ProviderTuning`] each construction was given, so the W-B
/// web-capability degrade is an OBSERVED build input rather than an
/// inference about a downstream request body.
struct TuningRecordingBuilder {
    tunings: StdMutex<Vec<(String, ProviderTuning)>>,
}

impl TuningRecordingBuilder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            tunings: StdMutex::new(Vec::new()),
        })
    }

    fn recorded(&self) -> Vec<(String, ProviderTuning)> {
        self.tunings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl AccountProviderBuilder for TuningRecordingBuilder {
    fn providers(&self) -> std::collections::BTreeSet<String> {
        haider_provider::BUILTIN_PROVIDER_NAMES
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn build(
        &self,
        _provider: &str,
        _credential: haider_accounts::SecretHandle,
        _model: &str,
        _alias: &CredentialAlias,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        Ok(Arc::new(haider_provider::FakeProvider::new(Vec::new())) as Arc<dyn Provider>)
    }

    fn build_tuned(
        &self,
        _profile: Option<&ProviderSummaryWire>,
        descriptor: &CredentialDescriptor,
        _credential: haider_accounts::SecretHandle,
        _model: &str,
        tuning: &ProviderTuning,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        self.tunings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((descriptor.provider.clone(), tuning.clone()));
        Ok(Arc::new(haider_provider::FakeProvider::new(Vec::new())) as Arc<dyn Provider>)
    }
}

/// LAW (W-B decision 1, "local fallback on refusal" — native half): an
/// ordinary turn is built with `web_tools` ON, so the pair declares its
/// provider-native web tools. Once the session latches the Anthropic
/// server-tool degrade, the NEXT Anthropic turn is built with `web_tools`
/// CLEARED — and the latch is pair-scoped: a session that switches to
/// OpenAI still gets that pair's own native web capability.
///
/// MUTATION CHECK: drop the `tuning.web_tools = false` assignment in
/// `AccountsProviderFactory::resolve_for_turn_with_web`. Expected runtime
/// failure: the third assertion below still sees `web_tools: true`.
#[tokio::test]
async fn anthropic_web_degrade_clears_the_native_declaration_for_anthropic_pairs_only() {
    let vault = Arc::new(MemoryVault::default());
    let anthropic_alias = CredentialAlias::new("anthropic-web-degrade");
    let openai_alias = CredentialAlias::new("openai-web-degrade");
    for alias in [&anthropic_alias, &openai_alias] {
        vault
            .put(alias, b"web-degrade-fixture-secret")
            .unwrap_or_else(|error| panic!("{error:?}"));
    }
    let snapshot = Arc::new(StdMutex::new(vec![
        CredentialDescriptor {
            alias: anthropic_alias,
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "anthropic fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
        CredentialDescriptor {
            alias: openai_alias,
            provider: OPENAI_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "openai fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
    ]));
    let builder = TuningRecordingBuilder::new();
    let factory = AccountsProviderFactory::new(
        snapshot,
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::clone(&builder) as Arc<dyn AccountProviderBuilder>,
    );
    let metadata = |provider: &str| haider_protocol::session::SessionMetadataV1 {
        cwd: "/tmp/haider-web-degrade".into(),
        provider: provider.into(),
        model: "web-test".into(),
        max_tokens: 64,
        permission_overrides: None,
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        created_at_ms: 1,
    };
    let clean = crate::worker::WebCapabilityDegrade::default();
    let latched = crate::worker::WebCapabilityDegrade {
        anthropic_web_tools: true,
        openai_alpha_search: false,
    };

    factory
        .resolve_for_turn_with_web(&metadata(ANTHROPIC_PROVIDER_NAME), clean)
        .await
        .expect("undegraded anthropic turn");
    factory
        .resolve_for_turn(&metadata(ANTHROPIC_PROVIDER_NAME))
        .await
        .expect("plain resolution keeps the native declaration");
    factory
        .resolve_for_turn_with_web(&metadata(ANTHROPIC_PROVIDER_NAME), latched)
        .await
        .expect("degraded anthropic turn");
    factory
        .resolve_for_turn_with_web(&metadata(OPENAI_PROVIDER_NAME), latched)
        .await
        .expect("the SAME latch on a different pair");

    let recorded = builder.recorded();
    assert_eq!(recorded.len(), 4);
    assert!(
        recorded[0].1.web_tools,
        "an undegraded anthropic turn declares its server web tools"
    );
    assert!(
        recorded[1].1.web_tools,
        "the plain resolution path is unchanged"
    );
    assert!(
        !recorded[2].1.web_tools,
        "a latched session stops declaring the server web tools"
    );
    assert!(
        recorded[3].1.web_tools,
        "the latch is anthropic-scoped: another pair keeps its own native search"
    );
}
