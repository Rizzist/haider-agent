//! Durable real-CLI counterparts of the source/configuration write-error packet.
use super::*;
use base64::Engine as _;
use serde_json::{Value, json};

fn assert_failed_account_write(output: &std::process::Output, message: &str, json_output: bool) {
    assert!(!output.status.success(), "{output:?}");
    if json_output {
        let error: Value = serde_json::from_slice(&output.stdout).expect("account error JSON");
        assert_eq!(error["schema"], "haider.account.error.v1", "{error}");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|text| text.contains(message)),
            "{error}"
        );
    } else {
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{output:?}"
        );
    }
}

fn rpc(endpoint: &Path, body: Value) -> Value {
    use std::os::unix::net::UnixStream;
    fn send(stream: &mut UnixStream, value: Value) {
        let bytes = serde_json::to_vec(&value).expect("encode frame");
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .expect("frame length");
        stream.write_all(&bytes).expect("frame body");
    }
    fn receive(stream: &mut UnixStream) -> Value {
        let mut length = [0; 4];
        stream.read_exact(&mut length).expect("frame length");
        let mut bytes = vec![0; (u32::from_be_bytes(length) & 0x7fff_ffff) as usize];
        stream.read_exact(&mut bytes).expect("frame body");
        serde_json::from_slice(&bytes).expect("JSON frame")
    }
    let mut stream = UnixStream::connect(endpoint).expect("connect owned daemon");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read deadline");
    send(
        &mut stream,
        json!({"v":1,"kind":"hello","protocol_min":1,"protocol_max":1,"client_name":"catalog-write-test","client_version":"test","client_instance_id":"catalog-write-test","client_kind":"headless","capabilities_requested":["view","control","account"],"max_receive_frame":16777216}),
    );
    assert_eq!(receive(&mut stream)["kind"], "welcome");
    send(
        &mut stream,
        json!({"v":1,"kind":"request","request_id":"catalog-test","body":body}),
    );
    loop {
        let response = receive(&mut stream);
        if response["kind"] == "response" && response["request_id"] == "catalog-test" {
            return response["body"].clone();
        }
    }
}
fn set_default(profile: &Path, endpoint: &Path, provider: &str, model: &str) {
    let revision = attached_json(profile, &["models", "--json"])["revision"].clone();
    let response = rpc(
        endpoint,
        json!({"method":"account.set_default_model","command_id":"catalog-default","provider":provider,"model":model,"expected_revision":revision}),
    );
    assert_eq!(
        response["method"], "account.set_default_model",
        "{response}"
    );
}
fn create(endpoint: &Path, workspace: &Path, provider: &str, model: &str, command: &str) -> Value {
    rpc(
        endpoint,
        json!({"method":"session.create","command_id":command,"cwd":workspace,"provider":provider,"model":model,"resolve_model":model.is_empty(),"max_tokens":4096,"interaction_mode":"autonomous"}),
    )
}
fn write_codex(root: &Path, who: &str) {
    std::fs::create_dir_all(root).expect("source root");
    let jwt = |claims: Value| {
        format!(
            "eyJhbGciOiJub25lIn0.{}.synthetic",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&claims).expect("claims"))
        )
    };
    let claims = json!({"sub":who,"email":format!("{who}@synthetic.invalid"),"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":who,"chatgpt_plan_type":"pro"}});
    let file = root.join("auth.json");
    std::fs::write(&file,json!({"account_id":who,"last_refresh":"2026-10-16T01:00:00Z","tokens":{"access_token":jwt(claims.clone()),"id_token":jwt(claims),"refresh_token":"synthetic-unused-refresh"}}).to_string()).expect("fake source");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
        .expect("source permissions");
}
fn source_write_case(readd: bool) {
    let mut owner = haider();
    owner.env_remove("HAIDER_TEST_FAKE_PROVIDER");
    let endpoint = configure_isolated_runtime(&mut owner);
    let source = owner._profile_root.path().join("codex-source");
    write_codex(&source, "synthetic-A");
    owner.args([
        "account",
        "source",
        "add",
        "codex_home",
        source.to_str().expect("root"),
    ]);
    let added = owner.output().expect("source add");
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    attached_json(&owner.profile, &["daemon", "stop", "--json"]);
    wait_for_profile_lock(&owner.profile, true);
    let descriptors: Vec<haider_protocol::credential::CredentialDescriptor> =
        serde_json::from_slice(
            &std::fs::read(owner.profile.join("accounts.json")).expect("accounts"),
        )
        .expect("descriptors");
    let a = descriptors
        .iter()
        .find(|d| d.provider == "openai-oauth" && d.active)
        .expect("A");
    let key = haider_store::ProviderModelCacheKey::for_account("openai-oauth", a);
    {
        let store = haider_store::Store::open(&owner.profile).expect("seed store");
        store.put_provider_models(key.as_str(),&json!([{"slug":"a-private-only","display_name":"A only","supported_efforts":[],"visible":true,"use_responses_lite":true}]).to_string(),None,1).expect("seed fetched row");
    }
    let positive = provider_snapshot_json(&owner.profile, "openai-oauth");
    assert!(
        positive["models"]
            .as_array()
            .expect("rows")
            .contains(&json!("a-private-only"))
    );
    set_default(&owner.profile, &endpoint, "openai-oauth", "a-private-only");
    let obstacle = owner.profile.join("credential_sources.json.tmp");
    std::fs::create_dir(&obstacle).expect("metadata obstacle");
    write_codex(&source, "synthetic-B");
    let args = if readd {
        vec![
            "account",
            "source",
            "add",
            "codex_home",
            source.to_str().expect("root"),
        ]
    } else {
        vec!["account", "source", "scan", "--json"]
    };
    let failed = attached_haider(&owner.profile)
        .args(args)
        .output()
        .expect("source mutation");
    assert_failed_account_write(&failed, "temporary credential source", !readd);
    let changed: Vec<haider_protocol::credential::CredentialDescriptor> = serde_json::from_slice(
        &std::fs::read(owner.profile.join("accounts.json")).expect("saved accounts"),
    )
    .expect("descriptors");
    assert!(
        changed
            .iter()
            .any(|b| b.alias == a.alias && b.incarnation > a.incarnation),
        "source change committed"
    );
    let current = provider_snapshot_json(&owner.profile, "openai-oauth");
    let admission = create(
        &endpoint,
        &owner._profile_root.path().join("workspace"),
        "openai-oauth",
        "a-private-only",
        "retired-source-selection",
    );
    std::fs::remove_dir(obstacle).expect("remove obstacle");
    attached_json(&owner.profile, &["account", "source", "scan", "--json"]);
    let recovery = provider_snapshot_json(&owner.profile, "openai-oauth");
    assert!(
        !current["models"]
            .as_array()
            .expect("rows")
            .contains(&json!("a-private-only"))
            && current["default_model"] != "a-private-only",
        "{current}"
    );
    assert_ne!(
        admission["method"], "session.create",
        "a retired row cannot be selected: {admission}"
    );
    assert!(
        !recovery["models"]
            .as_array()
            .expect("rows")
            .contains(&json!("a-private-only"))
            && recovery["default_model"] != "a-private-only",
        "{recovery}"
    );
}
#[test]
fn partial_write_cli_source_scan_retires_committed_owner() {
    source_write_case(false);
}
#[test]
fn partial_write_cli_existing_source_add_retires_committed_owner() {
    source_write_case(true);
}

#[test]
fn partial_write_cli_auth_journal_error_retires_rows_default_and_automatic_selection() {
    let mut owner = haider();
    owner
        .env_remove("HAIDER_TEST_FAKE_PROVIDER")
        .args(["models", "--json"]);
    let endpoint = configure_isolated_runtime(&mut owner);
    assert!(owner.output().expect("start daemon").status.success());
    let proxy = CredentialCatalogProxy::start();
    let add = spawn_keyed_account_command(
        &owner.profile,
        "add",
        "work",
        Some(&proxy.origin),
        "synthetic-A",
    )
    .wait_with_output()
    .expect("add account");
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    attached_json(&owner.profile, &["models", "--refresh", "work", "--json"]);
    let positive = provider_snapshot_json(&owner.profile, "work");
    assert!(
        positive["models"]
            .as_array()
            .expect("rows")
            .contains(&json!("fabricated-old"))
    );
    set_default(&owner.profile, &endpoint, "work", "fabricated-old");
    let session = create(
        &endpoint,
        &owner._profile_root.path().join("workspace"),
        "work",
        "fabricated-old",
        "before-auth-error",
    );
    assert_eq!(session["method"], "session.create", "{session}");
    let db = rusqlite::Connection::open(owner.profile.join("store.sqlite")).expect("database");
    db.execute_batch("CREATE TRIGGER catalog_auth_failure BEFORE INSERT ON events WHEN NEW.event_id LIKE '%provider-auth-%' BEGIN SELECT RAISE(ABORT, 'synthetic journal unavailable'); END;").expect("journal obstacle");
    proxy.set_mode(CredentialCatalogMode::NotFound);
    let failed = attached_haider(&owner.profile)
        .args(["account", "update", "work", "--no-auth", "--json"])
        .output()
        .expect("auth change");
    assert_failed_account_write(&failed, "synthetic journal unavailable", true);
    let current = provider_snapshot_json(&owner.profile, "work");
    assert_eq!(
        current["auth_methods"],
        json!([]),
        "saved anonymous profile"
    );
    let automatic = create(
        &endpoint,
        &owner._profile_root.path().join("workspace"),
        "work",
        "",
        "after-auth-error",
    );
    db.execute_batch("DROP TRIGGER catalog_auth_failure;")
        .expect("remove obstacle");
    attached_json(
        &owner.profile,
        &["account", "update", "work", "--no-auth", "--json"],
    );
    let retried = provider_snapshot_json(&owner.profile, "work");
    assert!(
        !current["models"]
            .as_array()
            .expect("rows")
            .contains(&json!("fabricated-old"))
            && current["default_model"] != "fabricated-old",
        "{current}"
    );
    assert_ne!(
        automatic["metadata"]["model"], "fabricated-old",
        "automatic sessions must not borrow the former account's default: {automatic}"
    );
    assert!(
        !retried["models"]
            .as_array()
            .expect("rows")
            .contains(&json!("fabricated-old")),
        "retry during discovery outage: {retried}"
    );
}
