use super::*;

const FIXTURE_STATE: &str = "fixture-state_971";
const FIXTURE_NONCE: &str = "fixture-nonce_971";
const FIXTURE_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const FIXTURE_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

fn canonical_token_form_query(url: &str) -> Vec<u8> {
    let parsed = Url::parse(url).expect("fixture URL");
    let mut pairs = parsed.query_pairs().into_owned().collect::<Vec<_>>();
    pairs.sort();
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
        .into_bytes()
}

/// Reference: openai/codex 2cbbf0c9b542a36a1c3284b5e804917635b6f666,
/// reconstructed by tests/fixtures/oauth/generate_codex_authorize_url.py.
/// Both builders use the same synthetic state and PKCE challenge. The only
/// Haider differences are the four OIDC scopes (without connector access),
/// scope spaces encoded as `+`, state before code_challenge, and an OIDC nonce
/// after code_challenge_method. Every transformation pins its original bytes;
/// no URL parsing, sorting or re-encoding may hide other reference drift.
#[test]
fn openai_authorize_url_matches_codex_reference_and_oidc_scope_golden() {
    let registration = OAuthProviderCatalog::default()
        .registration("openai-oauth")
        .expect("production OpenAI registration");
    assert_eq!(
        registration.redirect_policy,
        OAuthRedirectPolicy::OpenAiCodexLoopback
    );
    assert_eq!(registration.redirect_policy.ports(), &[1455, 1457]);
    assert_eq!(OAuthRedirectPolicy::EphemeralIpv4Loopback.ports(), &[0]);
    let (path, redirect, authority) = compose_redirect("openai-oauth", 1455, "unused");
    assert_eq!(path, "/auth/callback");
    assert_eq!(authority, "localhost:1455");
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(FIXTURE_VERIFIER)),
        FIXTURE_CHALLENGE
    );
    let actual = build_authorization_url(
        &registration,
        &redirect,
        FIXTURE_STATE,
        FIXTURE_CHALLENGE,
        FIXTURE_NONCE,
    );
    assert_eq!(
        actual.expose_authorization_url().as_bytes(),
        include_str!("../tests/fixtures/oauth/haider-openai-authorize-url.txt")
            .strip_suffix('\n')
            .expect("fixture line ending")
            .as_bytes()
    );

    let mut upstream = include_str!("../tests/fixtures/oauth/codex-authorize-url.txt")
        .strip_suffix('\n')
        .expect("fixture line ending")
        .to_owned();
    for (codex, haider) in [
        // Scope subset and its exact form-urlencoding; no other value changes.
        (
            "&scope=openid%20profile%20email%20offline_access%20api.connectors.read%20api.connectors.invoke&",
            "&scope=openid+profile+email+offline_access&",
        ),
        // Move only this state value from its pinned upstream position ...
        (
            "&codex_cli_simplified_flow=true&state=fixture-state_971&originator=",
            "&codex_cli_simplified_flow=true&originator=",
        ),
        // ... to its pinned Haider position immediately before the challenge.
        (
            "&scope=openid+profile+email+offline_access&code_challenge=",
            "&scope=openid+profile+email+offline_access&state=fixture-state_971&code_challenge=",
        ),
        // Haider alone supplies the nonce that its ID-token verifier checks.
        (
            "&code_challenge_method=S256&id_token_add_organizations=",
            "&code_challenge_method=S256&nonce=fixture-nonce_971&id_token_add_organizations=",
        ),
    ] {
        assert_eq!(upstream.matches(codex).count(), 1, "Codex drift: {codex}");
        upstream = upstream.replacen(codex, haider, 1);
    }
    assert_eq!(
        actual.expose_authorization_url().as_bytes(),
        upstream.as_bytes(),
        "full authorize URL must match the explicitly adapted Codex reference"
    );
}

#[test]
fn openai_token_exchange_has_exact_codex_form_fields_and_redirect() {
    let registration = OAuthProviderCatalog::default()
        .registration("openai-oauth")
        .expect("OpenAI");
    let (_, redirect, _) = compose_redirect("openai-oauth", 1455, "unused");
    let actual = authorization_code_request_body(
        &registration,
        b"fixture+code/971",
        FIXTURE_STATE.as_bytes(),
        FIXTURE_VERIFIER.as_bytes(),
        &redirect,
    )
    .expect("token body");
    assert_eq!(actual.content_type(), "application/x-www-form-urlencoded");
    // Query order is immaterial to form decoding; pin its exact byte encoding
    // separately from the upstream field-set comparison.
    assert_eq!(actual.as_ref(), concat!(
        "grant_type=authorization_code&code=fixture%2Bcode%2F971&client_id=app_EMoamEEZ73f0CkXaXp7hrann",
        "&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
        "&code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
    ).as_bytes());
    let upstream = concat!(
        "https://auth.openai.com/oauth/token?grant_type=authorization_code&code=fixture%2Bcode%2F971",
        "&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
        "&client_id=app_EMoamEEZ73f0CkXaXp7hrann",
        "&code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
    );
    assert_eq!(
        canonical_token_form_query(&format!(
            "{}?{}",
            registration.token_endpoint,
            std::str::from_utf8(actual.as_ref()).expect("form UTF-8")
        )),
        canonical_token_form_query(upstream)
    );
}

#[tokio::test]
async fn occupied_callback_port_falls_back_without_contacting_or_cancelling_incumbent() {
    let incumbent = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("incumbent");
    let occupied = incumbent.local_addr().expect("incumbent address").port();
    // Inject OS-assigned ports for isolation; production's exact [1455,1457]
    // list is pinned above and exercised by the real-daemon reproduction.
    let fallback = bind_callback_listener(&[occupied, 0])
        .await
        .expect("fallback");
    assert_ne!(
        fallback.local_addr().expect("fallback address").port(),
        occupied
    );
    assert_eq!(
        bind_callback_listener(&[occupied])
            .await
            .expect_err("no unregistered fallback")
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    // An existing server receives no /cancel request or other traffic.
    let accept = incumbent.accept();
    tokio::pin!(accept);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(accept.as_mut().poll(cx).is_pending()))
            .await
    );
}

#[test]
fn openai_registered_callback_rejects_stale_state_missing_code_and_foreign_authority() {
    let (path, _, authority) = compose_redirect("openai-oauth", 1455, "unused");
    let request =
        |host: &str, query: &str| format!("GET {path}?{query} HTTP/1.1\r\nHost: {host}\r\n\r\n");
    for query in [
        "state=old-attempt&code=synthetic",
        "state=fixture-state_971",
        "state=fixture-state_971&code=",
        "state=fixture-state_971&code=x&error=access_denied",
        "state=fixture-state%255F971&code=x",
    ] {
        assert!(
            matches!(
                parse_callback(
                    request(&authority, query).as_bytes(),
                    &path,
                    &authority,
                    FIXTURE_STATE.as_bytes()
                ),
                CallbackResult::Invalid(CallbackRejection::WrongAttempt)
            ),
            "{query}"
        );
    }
    for host in ["127.0.0.1:1455", "localhost:1457", "evil.example:1455"] {
        assert!(matches!(
            parse_callback(
                request(host, "state=fixture-state_971&code=x").as_bytes(),
                &path,
                &authority,
                FIXTURE_STATE.as_bytes()
            ),
            CallbackResult::Invalid(CallbackRejection::WrongAddress)
        ));
    }
    for (error, expected) in [
        ("access_denied", "access_denied"),
        ("server_error", "authorization_denied"),
    ] {
        assert!(
            matches!(parse_callback(request(&authority, &format!("state={FIXTURE_STATE}&error={error}")).as_bytes(),
            &path, &authority, FIXTURE_STATE.as_bytes()), CallbackResult::Denied(code) if code == expected)
        );
    }
    assert!(matches!(
        parse_callback(
            request(&authority, "state=fixture-state_971&error=unknown_error").as_bytes(),
            &path,
            &authority,
            FIXTURE_STATE.as_bytes()
        ),
        CallbackResult::Invalid(CallbackRejection::UnrecognizedProviderError)
    ));
}

/// The OpenAI identity adapter also reads compact JWT claims after the
/// injected verifier succeeds. Keep that production decoder exercised with
/// a synthetic compact token; all other fake-server tests retain raw JSON.
struct OpenAiFixtureIdentityVerifier;

#[async_trait::async_trait]
impl OAuthIdentityVerifier for OpenAiFixtureIdentityVerifier {
    async fn verify(
        &self,
        id_token: &[u8],
        expected: OAuthIdentityExpectation<'_>,
    ) -> Result<OAuthIdentityV1, OAuthPublicError> {
        let token = std::str::from_utf8(id_token).expect("synthetic token UTF-8");
        let payload = token
            .strip_prefix("fixture.")
            .and_then(|token| token.strip_suffix(".fixture"))
            .expect("synthetic compact token");
        let claims = URL_SAFE_NO_PAD.decode(payload).expect("fixture claims");
        FakeIdentityVerifier.verify(&claims, expected).await
    }
}

#[tokio::test]
async fn openai_localhost_callback_completes_pkce_and_nonce_validation_over_real_http() {
    for (mode, expected_failure) in [
        (FakeMode::Success, None),
        (FakeMode::NonceMismatch, Some("identity_claim_mismatch")),
        (FakeMode::VerifierMismatch, Some("invalid_grant")),
    ] {
        let server = FakeOAuthServer::start(mode, false).await;
        server.state.jwt_id_tokens.store(true, Ordering::SeqCst);
        let mut registration = server.registration(Arc::new(OpenAiFixtureIdentityVerifier));
        registration.provider_id = "openai-oauth".to_owned();
        registration.redirect_policy = OAuthProviderCatalog::default()
            .registration("openai-oauth")
            .expect("OpenAI registration")
            .redirect_policy;
        // Exercise begin_flow's production port selection, not just the
        // URL helper. A return to unconditional :0 binding must fail here.
        let (coordinator, mut receiver) =
            coordinator_for_registration(registration, Duration::from_secs(5)).await;
        let (flow_id, authorization, port) = started_flow(&mut receiver).await;
        assert!([1455, 1457].contains(&port), "unregistered port: {port}");
        let params = Url::parse(&authorization)
            .expect("URL")
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(
            params["redirect_uri"],
            format!("http://localhost:{port}/auth/callback")
        );
        let browser = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("browser");
        let response = browser
            .get(authorization)
            .send()
            .await
            .expect("real callback");
        assert_eq!(response.status().as_u16(), 200);
        let status = wait_ready(&coordinator, &flow_id).await;
        if let Some(expected) = expected_failure {
            assert!(
                matches!(status, OAuthFlowStatusWire::Failed { public_code } if public_code == expected)
            );
        } else {
            assert!(
                matches!(status, OAuthFlowStatusWire::Ready { .. }),
                "{status:?}"
            );
        }
        assert_eq!(server.state.token_calls.load(Ordering::SeqCst), 1);
        assert!(coordinator.shutdown().await);
    }
}
