//! Recovery must not reopen credentials owned by a desktop client.
use super::*;

#[tokio::test]
async fn android_recovered_import_healing_never_reads_or_rotates_source_credentials() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let mut accounts = memory_accounts();
    let alias = CredentialAlias::new(OPENAI_OAUTH_PROVIDER_NAME);
    let bundle = openai_import_test_bundle(b"synthetic-access", b"synthetic-refresh", 1);
    let descriptor = oauth_descriptor_for(OPENAI_OAUTH_PROVIDER_NAME, &alias, &bundle, true);
    accounts
        .add(descriptor.clone())
        .expect("recovered descriptor");
    let vault = Arc::new(MemoryVault::new());
    let encoded = bundle.encode().expect("synthetic bundle");
    vault.put(&alias, &encoded).expect("recovered vault");
    let snapshot = Arc::new(StdMutex::new(vec![descriptor.clone()]));
    let fences = RefreshFenceRegistry::default();
    let expected = OAuthRefreshFence {
        fence_epoch: fences.current(&alias),
        generation: bundle.generation,
        issuer: bundle.issuer,
        audience: bundle.audience,
        resource: bundle.resource,
        subject_hash: bundle.identity.subject_hash,
    };
    let native = Arc::new(StubAccountClaudeNative::unavailable());
    // No receipt means an on-device login, whose normal refresh remains eligible.
    assert!(matches!(
        handle_oauth_import_heal(
            &store,
            &mut accounts,
            vault.clone(),
            &snapshot,
            None,
            &HashSet::new(),
            &fences,
            &descriptor,
            &expected,
            native.clone()
        )
        .await
        .expect("native login"),
        OAuthImportHealResult::NotImported
    ));
    let identity = OAuthImportIdentity {
        source: "codex".into(),
        alias: alias.as_str().into(),
        provider: OPENAI_OAUTH_PROVIDER_NAME.into(),
        candidate: None,
    };
    let coordinates = identity.canonical_json().expect("coordinates");
    store
        .account_add_claim_receipt(
            "recovered-import".into(),
            blake3::hash(coordinates.as_bytes()).to_hex().to_string(),
            coordinates,
        )
        .await
        .expect("claim receipt");
    store
        .finalize_account_add_receipt(
            "recovered-import".into(),
            AccountAddReceiptResponse {
                descriptor: descriptor.clone(),
            },
        )
        .await
        .expect("commit receipt");
    reset_oauth_import_read_count();
    let error = handle_oauth_import_heal(
        &store,
        &mut accounts,
        vault.clone(),
        &snapshot,
        None,
        &HashSet::new(),
        &fences,
        &descriptor,
        &expected,
        native.clone(),
    )
    .await
    .expect_err("import recovery must stop before source I/O or refresh fallback");
    assert_eq!(error.code, ErrorCode::Unauthorized);
    assert!(error.message.contains("sign in on this device"));
    assert_eq!(native.reads(), 0);
    assert_eq!(oauth_import_read_count(), 0);
    assert_eq!(
        vault
            .resolve(&alias)
            .expect("retained vault")
            .expose_secret(),
        encoded.as_slice()
    );

    for source in ["codex", "claude-code"] {
        assert!(
            load_oauth_import_material_with_native(
                source,
                2,
                native.as_ref(),
                ClaudeNativeReadEvent::Significant
            )
            .is_err()
        );
    }
    assert!(matches!(
        load_claude_native_import_material(2, native.as_ref(), ClaudeNativeReadEvent::Ordinary),
        Err(ClaudeNativeImportError::Invalid(_))
    ));
    assert!(
        crate::oauth::load_claude_credential_input(
            &dir.path().join("synthetic-missing"),
            native.as_ref(),
            ClaudeNativeReadEvent::Ordinary
        )
        .is_err()
    );
    let access = crate::oauth::ClaudeNativeCredentialAccess::new(native.clone());
    assert!(access.read(ClaudeNativeReadEvent::Significant).is_err());
    assert!(access.probe().is_err());
    assert_eq!(native.reads(), 0);
    assert_eq!(oauth_import_read_count(), 0);
    store.close().await.expect("close");
}
