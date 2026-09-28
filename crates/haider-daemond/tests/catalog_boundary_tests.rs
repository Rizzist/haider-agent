fn private_write(
    path: impl AsRef<std::path::Path>,
    value: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    std::fs::write(path.as_ref(), value)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path.as_ref(), std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
mod support;
use haider_daemon::{AccountsDependencies, DaemonConfig, DaemonDependencies};
use haider_rpc::{
    ClientKind, CommandId, DEFAULT_FRAME_LIMIT, RequestBody, RequestId, ResponseBody, WireFrame,
};
use std::sync::Arc;
use support::{UdsClient, ready_with_dependencies, test_root};
struct SyntheticCatalog {
    hold: Arc<std::sync::atomic::AtomicBool>,
    arrived: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl haider_daemon::ProviderModelDiscoverer for SyntheticCatalog {
    async fn discover(
        &self,
        _source: haider_provider::CatalogSource,
        access: Option<&str>,
        _etag: Option<&str>,
    ) -> Result<haider_provider::DiscoveredCatalog, haider_provider::CatalogError> {
        let delayed = access.is_some_and(|v| v == "synthetic-A" || v.ends_with(".synthetic-A"))
            && self.hold.swap(false, std::sync::atomic::Ordering::SeqCst);
        if delayed {
            self.arrived.notify_one();
            self.release.notified().await;
        }
        match access {
   Some(v) if v == "synthetic-A" || v.ends_with(".synthetic-A") => Ok(haider_provider::DiscoveredCatalog { models: vec![serde_json::from_value(serde_json::json!({"slug":if delayed {"astra-import-A-late"} else {"astra-import-A-only"},"display_name":"Synthetic A only","supported_efforts":[],"visible":true,"use_responses_lite":true})).unwrap()],etag:Some("A".into()) }),
   Some(v) if v == "synthetic-B" || v.ends_with(".synthetic-B") => Err(haider_provider::CatalogError::Unavailable { reason:"provider does not serve a model list to this credential (403)".into() }),
   other => panic!("unexpected synthetic fixture token: {other:?}"),
  }
    }
}
async fn req(c: &mut UdsClient, id: &str, body: RequestBody) -> ResponseBody {
    c.send(
        &WireFrame::Request {
            request_id: RequestId::new(id),
            body,
        },
        DEFAULT_FRAME_LIMIT,
    )
    .await;
    tokio::time::timeout(support::DEADLINE, async {
        loop {
            if let WireFrame::Response { request_id, body } = c.receive().await {
                if request_id.as_str() == id {
                    return body;
                }
            }
        }
    })
    .await
    .unwrap()
}
async fn import_case(provider: &str, source: &str, path_var: &str, malformed_id_token: bool) {
    if std::env::var_os("HAIDER_CATALOG_BOUNDARY_CHILD").is_none() {
        // A child process owns the fake import paths. It cannot race any
        // other test's daemon or touch the host's credential environment.
        let fixture = test_root("catalog-import-env-");
        let fixture_path = fixture.path().join("import-source.json");
        let device_path = fixture.path().join("device_id");
        private_write(&device_path, b"11111111-1111-4111-8111-111111111111").unwrap();
        let name = match source {
            "kimi-code" => "kimi_import_must_not_reuse_prior_account_catalog",
            "grok-cli" => "grok_import_must_not_reuse_prior_account_catalog",
            "codex" if malformed_id_token => {
                "malformed_identity_codex_import_must_not_reuse_prior_account_catalog"
            }
            "codex" => "identityless_codex_import_must_not_reuse_prior_account_catalog",
            _ => panic!("unexpected import source"),
        };
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(name)
            .arg("--nocapture")
            .env("HAIDER_CATALOG_BOUNDARY_CHILD", "1")
            .env(path_var, &fixture_path)
            .env("HAIDER_KIMI_DEVICE_ID_PATH", &device_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{provider} child failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }
    let root = test_root("hastra-import-");
    let config = DaemonConfig::new("astra-synthetic", root.path().join("store"), root.path());
    let fixture_path = std::path::PathBuf::from(std::env::var(path_var).unwrap());
    let write = |who: &str| {
        let mut v =
            serde_json::json!({"access_token":format!("synthetic-{who}"),"expires_in":3600});
        if source == "kimi-code" {
            v["expires_at"] = serde_json::json!(4_102_444_800_u64);
            v["scope"] = serde_json::json!("kimi-code");
            v["token_type"] = serde_json::json!("Bearer");
        }
        if source == "codex" {
            v = serde_json::json!({"tokens":{"access_token":format!("eyJhbGciOiJub25lIn0.eyJleHAiOjQxMDI0NDQ4MDB9.synthetic-{who}"),"refresh_token":format!("synthetic-refresh-{who}")}});
            if malformed_id_token {
                v["tokens"]["id_token"] = serde_json::json!("malformed");
            }
        }
        private_write(&fixture_path, v.to_string()).unwrap();
    };
    let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let deps = || DaemonDependencies {
        accounts: AccountsDependencies {
            model_discoverer: Some(Arc::new(SyntheticCatalog {
                hold: hold.clone(),
                arrived: arrived.clone(),
                release: release.clone(),
            })),
            ..Default::default()
        },
        ..Default::default()
    };
    let task = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "astra-review",
        "astra-import",
        ClientKind::Cli,
    )
    .await;
    write("A");
    let a = req(
        &mut c,
        "import-a",
        RequestBody::AccountOAuthImport {
            command_id: CommandId::new("import-a"),
            source: source.into(),
        },
    )
    .await;
    let ResponseBody::AccountOAuthImport { descriptor: a, .. } = a else {
        panic!("import A: {a:?}")
    };
    let fetched = req(
        &mut c,
        "fetch-a",
        RequestBody::ProviderModelsRefresh {
            provider: provider.into(),
        },
    )
    .await;
    let ResponseBody::ProviderModelsRefresh { provider: p, .. } = fetched else {
        panic!("fetch A: {fetched:?}")
    };
    assert!(
        p.models.contains(&"astra-import-A-only".to_owned()),
        "A positive control: {p:?}"
    );
    hold.store(true, std::sync::atomic::Ordering::SeqCst);
    c.send(
        &WireFrame::Request {
            request_id: RequestId::new("late-A"),
            body: RequestBody::ProviderModelsRefresh {
                provider: provider.into(),
            },
        },
        DEFAULT_FRAME_LIMIT,
    )
    .await;
    tokio::time::timeout(support::DEADLINE, arrived.notified())
        .await
        .unwrap();
    write("B");
    let b = req(
        &mut c,
        "import-b",
        RequestBody::AccountOAuthImport {
            command_id: CommandId::new("import-b"),
            source: source.into(),
        },
    )
    .await;
    let ResponseBody::AccountOAuthImport { descriptor: b, .. } = b else {
        panic!("import B: {b:?}")
    };
    assert!(
        b.incarnation > a.incarnation,
        "a different import must mint an account incarnation"
    );
    let listed = req(
        &mut c,
        "list-b",
        RequestBody::ProviderList {
            provider: Some(provider.into()),
        },
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = listed else {
        panic!("list B: {listed:?}")
    };
    let immediately = providers
        .iter()
        .find(|p| p.provider == provider)
        .unwrap()
        .models
        .clone();
    release.notify_one();
    let late_reply = tokio::time::timeout(support::DEADLINE, async {
        loop {
            if let WireFrame::Response { request_id, body } = c.receive().await {
                if request_id.as_str() == "late-A" {
                    break body;
                }
            }
        }
    })
    .await
    .unwrap();
    let late_list = req(
        &mut c,
        "list-late",
        RequestBody::ProviderList {
            provider: Some(provider.into()),
        },
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = late_list else {
        panic!("list late A: {late_list:?}")
    };
    let after_late = providers
        .iter()
        .find(|p| p.provider == provider)
        .unwrap()
        .models
        .clone();
    let refreshed = req(
        &mut c,
        "fetch-b",
        RequestBody::ProviderModelsRefresh {
            provider: provider.into(),
        },
    )
    .await;
    let listed = req(
        &mut c,
        "list-b-after",
        RequestBody::ProviderList {
            provider: Some(provider.into()),
        },
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = listed else {
        panic!("list B after: {listed:?}")
    };
    let after = providers
        .iter()
        .find(|p| p.provider == provider)
        .unwrap()
        .models
        .clone();
    let ka = haider_store::ProviderModelCacheKey::for_account(provider, &a);
    let kb = haider_store::ProviderModelCacheKey::for_account(provider, &b);
    drop(c);
    task.shutdown_handle()
        .request("astra synthetic import restart");
    let _ = task.join().await;
    let restarted = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "astra-review",
        "astra-restart",
        ClientKind::Cli,
    )
    .await;
    let listed = req(
        &mut c,
        "list-restart",
        RequestBody::ProviderList {
            provider: Some(provider.into()),
        },
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = listed else {
        panic!("list restart: {listed:?}")
    };
    let after_restart = providers
        .iter()
        .find(|p| p.provider == provider)
        .unwrap()
        .models
        .clone();
    println!(
        "ASTRA_IMPORT_PROBE {}",
        serde_json::json!({"provider":provider,"descriptor_A":a,"descriptor_B":b,"same_key":ka==kb,"key_A":ka.as_str(),"key_B":kb.as_str(),"B_immediate_models":immediately,"B_after_forbidden_models":after,"B_after_late_A_models":after_late,"late_A_reply":format!("{late_reply:?}"),"refresh_B":format!("{refreshed:?}"),"B_first_restart_models":after_restart})
    );
    drop(c);
    restarted
        .shutdown_handle()
        .request("astra synthetic import probe done");
    let _ = restarted.join().await;
    let twice_restarted = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "catalog-boundary",
        "second-restart",
        ClientKind::Cli,
    )
    .await;
    let listed = req(
        &mut c,
        "list-second-restart",
        RequestBody::ProviderList {
            provider: Some(provider.into()),
        },
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = listed else {
        panic!("list second restart: {listed:?}")
    };
    let after_second_restart = providers
        .iter()
        .find(|p| p.provider == provider)
        .unwrap()
        .models
        .clone();
    drop(c);
    twice_restarted
        .shutdown_handle()
        .request("catalog boundary second restart done");
    let _ = twice_restarted.join().await;
    assert!(
        [
            &immediately,
            &after_late,
            &after,
            &after_restart,
            &after_second_restart
        ]
        .iter()
        .all(|rows| !rows.iter().any(|m| m.starts_with("astra-import-A-"))),
        "previous account catalog survived a different import"
    );
}
#[tokio::test]
async fn kimi_import_must_not_reuse_prior_account_catalog() {
    import_case("kimi-oauth", "kimi-code", "HAIDER_KIMI_CREDS_PATH", false).await;
}
#[tokio::test]
async fn grok_import_must_not_reuse_prior_account_catalog() {
    import_case("grok-oauth", "grok-cli", "HAIDER_GROK_AUTH_PATH", false).await;
}

struct DeniedRecoveryCatalog;
#[async_trait::async_trait]
impl haider_daemon::ProviderModelDiscoverer for DeniedRecoveryCatalog {
    async fn discover(
        &self,
        _source: haider_provider::CatalogSource,
        _access: Option<&str>,
        _etag: Option<&str>,
    ) -> Result<haider_provider::DiscoveredCatalog, haider_provider::CatalogError> {
        Err(haider_provider::CatalogError::Unauthorized)
    }
}

async fn recovery_case(remove: bool) {
    let root = test_root("hastra-recover-");
    let config = DaemonConfig::new("astra-recovery", root.path().join("store"), root.path());
    let deps = || DaemonDependencies {
        accounts: AccountsDependencies {
            model_discoverer: Some(Arc::new(DeniedRecoveryCatalog)),
            ..Default::default()
        },
        ..Default::default()
    };
    let task = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "astra-review",
        "astra-recovery",
        ClientKind::Cli,
    )
    .await;
    for who in ["A", "B"] {
        let source = root.path().join(who);
        std::fs::create_dir_all(source.join("credentials")).unwrap();
        private_write(source.join("credentials/kimi-code.json"),serde_json::json!({"access_token":format!("synthetic-{who}"),"expires_at":4_102_444_800_u64,"scope":"kimi-code","token_type":"Bearer"}).to_string()).unwrap();
        let r = req(
            &mut c,
            &format!("add-{who}"),
            RequestBody::AccountSourceAdd {
                command_id: CommandId::new(format!("add-{who}")),
                kind: "kimi_code_home".into(),
                root: source.display().to_string(),
                label: Some(who.into()),
            },
        )
        .await;
        assert!(
            matches!(r, ResponseBody::AccountSourceAdd { .. }),
            "add: {r:?}"
        );
    }
    drop(c);
    task.shutdown_handle()
        .request("stage durable recovery fixture");
    let _ = task.join().await;
    let descriptors: Vec<haider_protocol::credential::CredentialDescriptor> =
        serde_json::from_slice(&std::fs::read(config.store_dir.join("accounts.json")).unwrap())
            .unwrap();
    let a = descriptors
        .iter()
        .find(|d| d.provider == "kimi-oauth" && d.active)
        .unwrap();
    let b = descriptors
        .iter()
        .find(|d| d.provider == "kimi-oauth" && !d.active)
        .unwrap();
    let ka = haider_store::ProviderModelCacheKey::for_account("kimi-oauth", a);
    let kb = haider_store::ProviderModelCacheKey::for_account("kimi-oauth", b);
    assert_ne!(
        ka, kb,
        "different aliases/accounts are positive control for key separation"
    );
    {
        let store = haider_store::Store::open(&config.store_dir).unwrap();
        store.put_provider_models(ka.as_str(),&serde_json::json!([{"slug":"astra-recovery-A-only","display_name":"A only","supported_efforts":[],"visible":true}]).to_string(),Some("A"),1).unwrap();
        if remove {
            let request =
                serde_json::json!({"alias":a.alias.as_str(),"expected_revision":null}).to_string();
            let recovery =
                serde_json::json!({"provider":"kimi-oauth","was_active":true}).to_string();
            store
                .account_remove_claim_receipt::<serde_json::Value>(
                    "astra-pending-remove",
                    blake3::hash(request.as_bytes()).to_hex().as_str(),
                    &request,
                    &recovery,
                    None,
                    a.alias.as_str(),
                    "kimi-oauth",
                    true,
                )
                .unwrap();
        } else {
            let request = serde_json::json!({"alias":b.alias.as_str()}).to_string();
            let recovery =
                serde_json::json!({"provider":"kimi-oauth","prior_alias":a.alias.as_str()})
                    .to_string();
            store
                .management_claim_receipt::<serde_json::Value>(
                    "astra-pending-select",
                    "account.set_active",
                    blake3::hash(request.as_bytes()).to_hex().as_str(),
                    &request,
                    Some(&recovery),
                    None,
                )
                .unwrap();
        }
    }
    let task = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "astra-review",
        "astra-recovery-after",
        ClientKind::Cli,
    )
    .await;
    let listed = req(
        &mut c,
        "recovered-list",
        RequestBody::ProviderList {
            provider: Some("kimi-oauth".into()),
        },
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = listed else {
        panic!("list after recovery: {listed:?}")
    };
    let rows = providers
        .iter()
        .find(|p| p.provider == "kimi-oauth")
        .unwrap()
        .models
        .clone();
    let recovered: Vec<haider_protocol::credential::CredentialDescriptor> =
        serde_json::from_slice(&std::fs::read(config.store_dir.join("accounts.json")).unwrap())
            .unwrap();
    let active = recovered
        .iter()
        .find(|d| d.provider == "kimi-oauth" && d.active)
        .unwrap();
    println!(
        "ASTRA_RECOVERY_PROBE {}",
        serde_json::json!({"pending_operation":if remove {"remove_A"} else {"select_B"},"alias_A":a.alias,"alias_B":b.alias,"active_after_recovery":active.alias,"key_A":ka.as_str(),"key_B":kb.as_str(),"models_after_recovery":rows})
    );
    assert_eq!(
        active.alias, b.alias,
        "pending operation actually recovered B"
    );
    drop(c);
    task.shutdown_handle().request("astra recovery probe done");
    let _ = task.join().await;
    assert!(
        !rows.contains(&"astra-recovery-A-only".to_owned()),
        "B inherited A's live rows because hydration preceded account recovery"
    );
}
#[tokio::test]
async fn pending_select_recovery_must_not_show_old_catalog() {
    recovery_case(false).await;
}
#[tokio::test]
async fn pending_remove_recovery_must_not_show_old_catalog() {
    recovery_case(true).await;
}

struct SyntheticApiBoundary;
#[async_trait::async_trait]
impl haider_daemon::ProviderModelDiscoverer for SyntheticApiBoundary {
    async fn discover(
        &self,
        source: haider_provider::CatalogSource,
        access: Option<&str>,
        _etag: Option<&str>,
    ) -> Result<haider_provider::DiscoveredCatalog, haider_provider::CatalogError> {
        if matches!(source, haider_provider::CatalogSource::OpenAiCompatible { ref origin }
            if origin.contains(":18081/"))
            || access.is_none()
        {
            return Err(haider_provider::CatalogError::Unauthorized);
        }
        let slugs: &[&str] = match access {
            Some("synthetic-api-A") => &["astra-api-A-only"],
            Some("synthetic-api-B") => &["astra-api-B-only", "astra-api-B-private-extra"],
            _ => panic!("unexpected synthetic API fixture"),
        };
        Ok(haider_provider::DiscoveredCatalog {models:slugs.iter().map(|slug| serde_json::from_value(serde_json::json!({"slug":slug,"display_name":slug,"supported_efforts":[],"visible":true})).unwrap()).collect(),etag:None})
    }
}
#[async_trait::async_trait]
impl haider_daemon::CredentialValidator for SyntheticApiBoundary {
    fn supports(&self, _provider: &str) -> bool {
        true
    }
    async fn validate(
        &self,
        _provider: &str,
        _model: &str,
        _secret: &[u8],
        _endpoint: Option<&str>,
    ) -> Result<haider_daemon::ValidatedIdentity, haider_daemon::ValidationError> {
        Ok(haider_daemon::ValidatedIdentity {
            identity: "synthetic-api-user-A".into(),
        })
    }
}
#[async_trait::async_trait]
impl haider_daemon::ProviderEndpointValidator for SyntheticApiBoundary {
    async fn validate(&self, origin: &str) -> Result<String, haider_protocol::error::HaiderError> {
        assert!(origin.starts_with("http://127.0.0.1:"));
        Ok(origin.into())
    }
}
async fn api_stage(c: &mut UdsClient, id: &str, secret: &str) -> String {
    let r = req(
        c,
        id,
        RequestBody::VaultStage {
            stage_id: id.into(),
            purpose: haider_rpc::StagePurpose::ApiKey,
            secret: haider_rpc::SecretWire::new(secret),
        },
    )
    .await;
    let ResponseBody::VaultStage {
        vault_reference, ..
    } = r
    else {
        panic!("stage: {r:?}")
    };
    vault_reference
}
async fn api_configure(
    c: &mut UdsClient,
    id: &str,
    provider: &str,
    model: &str,
    stage: Option<String>,
) {
    let r = req(
        c,
        &format!("revision-{id}"),
        RequestBody::ProviderList { provider: None },
    )
    .await;
    let ResponseBody::ProviderList { revision, .. } = r else {
        panic!("revision: {r:?}")
    };
    let r = req(
        c,
        id,
        RequestBody::ProviderConfigure {
            command_id: CommandId::new(id),
            provider: provider.into(),
            api_family: Some(if provider == "astra-anthropic-boundary" {
                haider_rpc::ProviderApiFamilyWire::AnthropicMessages
            } else {
                haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions
            }),
            origin: (provider == "astra-api-boundary" || provider == "astra-anthropic-boundary")
                .then(|| "http://127.0.0.1:18080/v1".into()),
            auth_requirement: Some(haider_rpc::ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec![model.into()],
            default_model: Some(model.into()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            probe_vault_reference: stage,
            trust: None,
            expected_revision: revision,
        },
    )
    .await;
    assert!(
        matches!(r, ResponseBody::ProviderConfigure { .. }),
        "configure: {r:?}"
    );
}
async fn staged_api_case(provider: &str) {
    let root = test_root("hastra-api-");
    let config = DaemonConfig::new("astra-api", root.path().join("store"), root.path());
    let deps = || DaemonDependencies {
        accounts: AccountsDependencies {
            model_discoverer: Some(Arc::new(SyntheticApiBoundary)),
            validator: Arc::new(SyntheticApiBoundary),
            provider_endpoint_validator: Arc::new(SyntheticApiBoundary),
            ..Default::default()
        },
        ..Default::default()
    };
    let task = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "astra-review",
        "astra-api",
        ClientKind::Cli,
    )
    .await;
    api_configure(&mut c, "configure-A", provider, "astra-api-A-only", None).await;
    let a_ref = api_stage(&mut c, "stage-A", "synthetic-api-A").await;
    let r = req(
        &mut c,
        "login-A",
        RequestBody::AccountLoginApi {
            command_id: CommandId::new("login-A"),
            provider: provider.into(),
            alias: Some("astra-api-account-A".into()),
            vault_reference: a_ref,
            validation_model: Some("astra-api-A-only".into()),
            replace_existing: false,
        },
    )
    .await;
    let ResponseBody::AccountLoginApi { descriptor: a } = r else {
        panic!("login A: {r:?}")
    };
    let r = req(
        &mut c,
        "fetch-A",
        RequestBody::ProviderModelsRefresh {
            provider: provider.into(),
        },
    )
    .await;
    let ResponseBody::ProviderModelsRefresh { provider: p, .. } = r else {
        panic!("fetch A: {r:?}")
    };
    assert!(
        p.models.contains(&"astra-api-A-only".into()),
        "A positive control: {p:?}"
    );
    let b_ref = api_stage(&mut c, "stage-B", "synthetic-api-B").await;
    api_configure(
        &mut c,
        "configure-B",
        provider,
        "astra-api-B-only",
        Some(b_ref),
    )
    .await;
    let r = req(
        &mut c,
        "list-after-B",
        RequestBody::ProviderList {
            provider: Some(provider.into()),
        },
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = r else {
        panic!("list B: {r:?}")
    };
    let rows = providers[0].models.clone();
    drop(c);
    task.shutdown_handle()
        .request("astra API provenance probe done");
    let _ = task.join().await;
    let descriptors: Vec<haider_protocol::credential::CredentialDescriptor> =
        serde_json::from_slice(&std::fs::read(config.store_dir.join("accounts.json")).unwrap())
            .unwrap();
    let active = descriptors
        .iter()
        .find(|d| d.provider == provider && d.active)
        .unwrap();
    assert_eq!(active.alias, a.alias);
    let key = haider_store::ProviderModelCacheKey::for_account(provider, active);
    let store = haider_store::Store::open(&config.store_dir).unwrap();
    let cached = store.provider_models(key.as_str()).unwrap().unwrap();
    println!(
        "ASTRA_API_PROBE {}",
        serde_json::json!({"provider":provider,"active_alias":active.alias,"active_account_key":key.as_str(),"models_after_staged_B":rows,"configured_request_models":["astra-api-B-only"],"active_A_cache_rows":serde_json::from_str::<serde_json::Value>(&cached.models_json).unwrap()})
    );
    assert!(
        !rows.contains(&"astra-api-B-private-extra".into())
            && !cached.models_json.contains("astra-api-B-private-extra"),
        "staged B catalog was published and cached under active A"
    );
}

#[tokio::test]
async fn staged_api_probe_must_not_be_cached_for_another_active_account() {
    staged_api_case("astra-api-boundary").await;
}
#[tokio::test]
async fn staged_anthropic_probe_must_not_be_cached_for_another_active_account() {
    staged_api_case("astra-anthropic-boundary").await;
}
#[tokio::test]
async fn staged_deepseek_probe_must_not_be_cached_for_another_active_account() {
    staged_api_case("deepseek").await;
}
#[tokio::test]
async fn staged_xai_probe_must_not_be_cached_for_another_active_account() {
    staged_api_case("xai").await;
}

fn active_boundary_descriptor(
    config: &DaemonConfig,
    provider: &str,
) -> haider_protocol::credential::CredentialDescriptor {
    let descriptors: Vec<haider_protocol::credential::CredentialDescriptor> =
        serde_json::from_slice(&std::fs::read(config.store_dir.join("accounts.json")).unwrap())
            .unwrap();
    descriptors
        .into_iter()
        .find(|row| row.provider == provider && row.active)
        .unwrap()
}

async fn reconfigure_boundary(
    c: &mut UdsClient,
    id: &str,
    provider: &str,
    origin: &str,
    auth: haider_rpc::ProviderAuthRequirementWire,
) {
    let ResponseBody::ProviderList { revision, .. } = req(
        c,
        &format!("revision-{id}"),
        RequestBody::ProviderList { provider: None },
    )
    .await
    else {
        panic!("provider revision unavailable")
    };
    let response = req(
        c,
        id,
        RequestBody::ProviderConfigure {
            command_id: CommandId::new(id),
            provider: provider.into(),
            api_family: Some(haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some(origin.into()),
            auth_requirement: Some(auth),
            enabled: true,
            models: vec!["explicit-static".into()],
            default_model: Some("explicit-static".into()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            probe_vault_reference: None,
            trust: None,
            expected_revision: revision,
        },
    )
    .await;
    assert!(
        matches!(response, ResponseBody::ProviderConfigure { .. }),
        "{response:?}"
    );
}

async fn configuration_boundary_case(repoint: bool) {
    let root = test_root("hastra-config-boundary-");
    let config = DaemonConfig::new(
        "astra-config-boundary",
        root.path().join("store"),
        root.path(),
    );
    let deps = || DaemonDependencies {
        accounts: AccountsDependencies {
            model_discoverer: Some(Arc::new(SyntheticApiBoundary)),
            validator: Arc::new(SyntheticApiBoundary),
            provider_endpoint_validator: Arc::new(SyntheticApiBoundary),
            ..Default::default()
        },
        ..Default::default()
    };
    let task = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "astra-review",
        "astra-config-boundary",
        ClientKind::Cli,
    )
    .await;
    let provider = "astra-api-boundary";
    api_configure(&mut c, "configure-A", provider, "astra-api-A-only", None).await;
    let stage = api_stage(&mut c, "stage-A", "synthetic-api-A").await;
    let response = req(
        &mut c,
        "login-A",
        RequestBody::AccountLoginApi {
            command_id: CommandId::new("login-A"),
            provider: provider.into(),
            alias: Some("same-key-same-alias".into()),
            vault_reference: stage,
            validation_model: Some("astra-api-A-only".into()),
            replace_existing: false,
        },
    )
    .await;
    assert!(
        matches!(response, ResponseBody::AccountLoginApi { .. }),
        "{response:?}"
    );
    let response = req(
        &mut c,
        "fetch-A",
        RequestBody::ProviderModelsRefresh {
            provider: provider.into(),
        },
    )
    .await;
    assert!(
        matches!(response, ResponseBody::ProviderModelsRefresh { .. }),
        "{response:?}"
    );
    let first = active_boundary_descriptor(&config, provider);
    let old_key = haider_store::ProviderModelCacheKey::for_account(provider, &first);
    let new_origin = if repoint {
        "http://127.0.0.1:18081/v1"
    } else {
        "http://127.0.0.1:18080/v1"
    };
    let new_auth = if repoint {
        haider_rpc::ProviderAuthRequirementWire::ApiKey
    } else {
        haider_rpc::ProviderAuthRequirementWire::None
    };
    reconfigure_boundary(&mut c, "configure-boundary", provider, new_origin, new_auth).await;
    let second = active_boundary_descriptor(&config, provider);
    assert!(second.incarnation > first.incarnation);
    let ResponseBody::ProviderList { providers, .. } = req(
        &mut c,
        "list-boundary",
        RequestBody::ProviderList {
            provider: Some(provider.into()),
        },
    )
    .await
    else {
        panic!("provider list unavailable")
    };
    assert!(providers[0].models.contains(&"explicit-static".into()));
    assert!(!providers[0].models.contains(&"astra-api-A-only".into()));
    if !repoint {
        reconfigure_boundary(
            &mut c,
            "configure-key-again",
            provider,
            new_origin,
            haider_rpc::ProviderAuthRequirementWire::ApiKey,
        )
        .await;
        let third = active_boundary_descriptor(&config, provider);
        assert!(third.incarnation > second.incarnation);
    }
    drop(c);
    task.shutdown_handle()
        .request("configuration boundary test restart");
    let _ = task.join().await;
    let store = haider_store::Store::open(&config.store_dir).unwrap();
    assert!(store.provider_models(old_key.as_str()).unwrap().is_none());
    drop(store);
    let task = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "astra-review",
        "astra-config-boundary",
        ClientKind::Cli,
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = req(
        &mut c,
        "list-restart",
        RequestBody::ProviderList {
            provider: Some(provider.into()),
        },
    )
    .await
    else {
        panic!("provider list unavailable after restart")
    };
    if repoint {
        assert!(!providers[0].models.contains(&"astra-api-A-only".into()));
    }
    drop(c);
    task.shutdown_handle()
        .request("configuration boundary test done");
    let _ = task.join().await;
}

#[tokio::test]
async fn auth_mode_none_and_back_to_api_key_invalidates_private_catalog() {
    configuration_boundary_case(false).await;
}

#[tokio::test]
async fn endpoint_repoint_same_key_failed_discovery_and_restart_keep_old_catalog_hidden() {
    configuration_boundary_case(true).await;
}

struct SourceRaceCatalog {
    arrived: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl haider_daemon::ProviderModelDiscoverer for SourceRaceCatalog {
    async fn discover(
        &self,
        _source: haider_provider::CatalogSource,
        access: Option<&str>,
        _etag: Option<&str>,
    ) -> Result<haider_provider::DiscoveredCatalog, haider_provider::CatalogError> {
        let slug = match access {
            Some("synthetic-A") => "astra-source-A-only",
            Some("synthetic-B") => {
                self.arrived.notify_one();
                self.release.notified().await;
                "astra-source-B-only"
            }
            _ => panic!("unexpected synthetic source race token"),
        };
        Ok(haider_provider::DiscoveredCatalog {models:vec![serde_json::from_value(serde_json::json!({"slug":slug,"display_name":slug,"supported_efforts":[],"visible":true,"use_responses_lite":true})).unwrap()],etag:None})
    }
}
#[tokio::test]
async fn read_through_credential_must_match_catalog_identity_snapshot() {
    let root = test_root("hastra-source-race-");
    let config = DaemonConfig::new("astra-race", root.path().join("store"), root.path());
    let source = root.path().join("source");
    std::fs::create_dir_all(source.join("credentials")).unwrap();
    let file = source.join("credentials/kimi-code.json");
    let write = |who: &str, modified: Option<std::time::SystemTime>| {
        private_write(&file,serde_json::json!({"access_token":format!("synthetic-{who}"),"expires_at":4_102_444_800_u64,"scope":"kimi-code","token_type":"Bearer"}).to_string()).unwrap();
        if let Some(t) = modified {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&file)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(t))
                .unwrap();
        }
    };
    write("A", None);
    let a_time = std::fs::metadata(&file).unwrap().modified().unwrap();
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let deps = || DaemonDependencies {
        accounts: AccountsDependencies {
            model_discoverer: Some(Arc::new(SourceRaceCatalog {
                arrived: arrived.clone(),
                release: release.clone(),
            })),
            ..Default::default()
        },
        ..Default::default()
    };
    let task = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "astra-review",
        "astra-race",
        ClientKind::Cli,
    )
    .await;
    let r = req(
        &mut c,
        "race-add",
        RequestBody::AccountSourceAdd {
            command_id: CommandId::new("race-add"),
            kind: "kimi_code_home".into(),
            root: source.display().to_string(),
            label: None,
        },
    )
    .await;
    assert!(
        matches!(r, ResponseBody::AccountSourceAdd { .. }),
        "source add: {r:?}"
    );
    let read_descriptor = || -> haider_protocol::credential::CredentialDescriptor {
        let ds: Vec<haider_protocol::credential::CredentialDescriptor> =
            serde_json::from_slice(&std::fs::read(config.store_dir.join("accounts.json")).unwrap())
                .unwrap();
        ds.into_iter()
            .find(|d| d.provider == "kimi-oauth" && d.active)
            .unwrap()
    };
    let a = read_descriptor();
    let key_a = haider_store::ProviderModelCacheKey::for_account("kimi-oauth", &a);
    let r = req(
        &mut c,
        "race-fetch-A",
        RequestBody::ProviderModelsRefresh {
            provider: "kimi-oauth".into(),
        },
    )
    .await;
    assert!(
        matches!(r, ResponseBody::ProviderModelsRefresh { .. }),
        "A positive control: {r:?}"
    );
    write("B", Some(a_time + std::time::Duration::from_secs(1)));
    c.send(
        &WireFrame::Request {
            request_id: RequestId::new("race-fetch-B"),
            body: RequestBody::ProviderModelsRefresh {
                provider: "kimi-oauth".into(),
            },
        },
        DEFAULT_FRAME_LIMIT,
    )
    .await;
    // Give the broker a bounded window to observe B, shorter than the
    // source watcher's coalescing period. A correct implementation rejects
    // B before the discoverer sees it; the old implementation blocks there.
    let reached_discoverer =
        tokio::time::timeout(std::time::Duration::from_millis(75), arrived.notified())
            .await
            .is_ok();
    write("A", Some(a_time));
    release.notify_one();
    let reply = tokio::time::timeout(support::DEADLINE, async {
        loop {
            if let WireFrame::Response { request_id, body } = c.receive().await {
                if request_id.as_str() == "race-fetch-B" {
                    break body;
                }
            }
        }
    })
    .await
    .unwrap();
    let after = read_descriptor();
    let key_after = haider_store::ProviderModelCacheKey::for_account("kimi-oauth", &after);
    let r = req(
        &mut c,
        "race-models",
        RequestBody::ProviderList {
            provider: Some("kimi-oauth".into()),
        },
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = r else {
        panic!("race models: {r:?}")
    };
    let rows = providers[0].models.clone();
    drop(c);
    task.shutdown_handle().request("astra source race restart");
    let _ = task.join().await;
    let task = ready_with_dependencies(&config, deps()).await;
    let mut c = UdsClient::connect_control(
        &config.endpoint_path(),
        DEFAULT_FRAME_LIMIT,
        "astra-review",
        "astra-race-restart",
        ClientKind::Cli,
    )
    .await;
    let r = req(
        &mut c,
        "race-restart-models",
        RequestBody::ProviderList {
            provider: Some("kimi-oauth".into()),
        },
    )
    .await;
    let ResponseBody::ProviderList { providers, .. } = r else {
        panic!("race restart: {r:?}")
    };
    let restart_rows = providers[0].models.clone();
    println!(
        "ASTRA_SOURCE_RACE_PROBE {}",
        serde_json::json!({"descriptor_A":a,"descriptor_after_roundtrip":after,"same_key":key_a==key_after,"reached_B_discoverer":reached_discoverer,"A_models_after_B_fetch":rows,"A_models_after_restart":restart_rows,"B_fetch_reply":format!("{reply:?}")})
    );
    drop(c);
    task.shutdown_handle().request("astra source race done");
    let _ = task.join().await;
    assert_eq!(
        key_a, key_after,
        "fixture race inconclusive: watcher adopted B before it could be restored"
    );
    assert!(
        !reached_discoverer,
        "B's read-through credential reached discovery under A's descriptor"
    );
    assert!(
        matches!(reply, ResponseBody::Error { .. }),
        "the changed linked credential must abort this A-generation refresh: {reply:?}"
    );
    assert!(
        !rows.contains(&"astra-source-B-only".into())
            && !restart_rows.contains(&"astra-source-B-only".into()),
        "B catalog crossed into restored A's identity"
    );
}

#[tokio::test]
async fn identityless_codex_import_must_not_reuse_prior_account_catalog() {
    import_case("openai-oauth", "codex", "HAIDER_CODEX_AUTH_PATH", false).await;
}

#[tokio::test]
async fn malformed_identity_codex_import_must_not_reuse_prior_account_catalog() {
    import_case("openai-oauth", "codex", "HAIDER_CODEX_AUTH_PATH", true).await;
}
