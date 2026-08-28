#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use haider_accounts::MemoryVault;
use haider_protocol::ids::SessionId;
use haider_rpc::{SshProfileUpdateWire, SshScopeWire};

use super::*;

fn store() -> SshProfileStore {
    SshProfileStore::new(Arc::new(MemoryVault::default()))
}

fn profile(name: &str, auth: SshAuth) -> SshProfile {
    SshProfile {
        name: name.into(),
        description: Some("production build host".into()),
        ssh: SshTarget {
            host: "build.example.test".into(),
            port: 22,
            user: "builder".into(),
            auth,
            default_cwd: Some("/srv/build".into()),
            host_key: None,
        },
        last_used_ms: None,
    }
}

#[test]
fn profile_secret_store_crud_has_typed_errors_and_public_projection_has_no_secret() {
    let store = store();
    let secret = b"ssh-test-password-never-serialize";
    let vault_ref = store
        .put_auth_secret("prod", secret)
        .expect("store auth secret");
    let added = store
        .add(profile("prod", SshAuth::Password { vault_ref }))
        .expect("add profile");
    assert!(matches!(
        store.add(added.clone()),
        Err(SshError::SshProfileExists { name }) if name == "prod"
    ));

    let public = serde_json::to_vec(&added.public()).expect("serialize public projection");
    assert!(!public.windows(secret.len()).any(|window| window == secret));
    assert!(!String::from_utf8_lossy(&public).contains("haider.ssh.secret"));

    let updated = store
        .update_non_secret(
            "prod",
            SshProfileUpdateWire {
                description: Some(Some("updated".into())),
                ..SshProfileUpdateWire::default()
            },
            None,
        )
        .expect("update profile");
    assert_eq!(updated.description.as_deref(), Some("updated"));
    store.remove("prod").expect("remove profile");
    assert!(matches!(
        store.get("prod"),
        Err(SshError::SshProfileNotFound { .. })
    ));
}

#[test]
fn auth_secret_references_are_bound_to_their_profile() {
    let store = store();
    let vault_ref = store
        .put_auth_secret("prod", b"profile-bound-secret")
        .expect("store auth secret");
    assert!(store.resolve_auth_secret("prod", &vault_ref).is_ok());
    assert!(matches!(
        store.resolve_auth_secret("stage", &vault_ref),
        Err(SshError::StoreCorrupt { .. })
    ));
}

#[test]
fn internal_auth_debug_never_exposes_paths_or_vault_references() {
    let auth = SshAuth::KeyFile {
        path: "/secret/location/id_ed25519".into(),
        passphrase_vault_ref: Some("haider.ssh.secret.prod.sentinel".into()),
    };
    let debug = format!("{auth:?}");
    assert!(!debug.contains("/secret/location"));
    assert!(!debug.contains("sentinel"));

    let debug = format!(
        "{:?}",
        SshAuth::Password {
            vault_ref: "haider.ssh.secret.prod.password-sentinel".into(),
        }
    );
    assert!(!debug.contains("password-sentinel"));
}

#[test]
fn profile_names_and_scope_are_fail_closed() {
    for invalid in ["", "UPPER", "space name", "a/b", &"x".repeat(33)] {
        assert!(matches!(
            super::store::validate_name(invalid),
            Err(SshError::SshProfileInvalidName { .. })
        ));
    }
    let session = SessionId::new("session-scope");
    let all = SshScope::from_wire(SshScopeWire::All).expect("all scope");
    let none = SshScope::from_wire(SshScopeWire::None).expect("none scope");
    let allow = SshScope::from_wire(SshScopeWire::Allow {
        names: vec!["prod".into(), "stage".into()],
    })
    .expect("allow scope");
    assert!(all.allows("anything"));
    assert!(!none.allows("prod"));
    assert!(allow.allows("prod"));
    assert!(!allow.allows("secret-third-host"));
    assert_eq!(
        SshError::SshProfileOutOfScope {
            session_id: session,
            name: "secret-third-host".into(),
        }
        .code(),
        "ssh_profile_out_of_scope"
    );
    assert_eq!(
        allow,
        SshScope::Allow(BTreeSet::from(["prod".into(), "stage".into()]))
    );
}

#[test]
fn narrowed_session_scope_survives_a_store_reopen() {
    let vault = Arc::new(MemoryVault::default());
    let session = SessionId::new("session-durable-scope");
    let store = SshProfileStore::new(vault.clone());
    let scope = SshScope::Allow(BTreeSet::from(["prod".into()]));
    store
        .set_session_scope(&session, &scope)
        .expect("persist scope");
    let reopened = SshProfileStore::new(vault);
    assert_eq!(
        reopened.session_scope(&session).expect("reload scope"),
        scope
    );
    assert_eq!(
        reopened
            .session_scope(&SessionId::new("legacy-session"))
            .expect("legacy default"),
        SshScope::All
    );
}

#[test]
fn tofu_pins_once_and_a_changed_key_is_a_typed_refusal() {
    let store = store();
    store
        .add(profile("prod", SshAuth::Agent))
        .expect("add profile");
    let original = PinnedHostKey {
        algorithm: "ssh-ed25519".into(),
        fingerprint: "SHA256:first".into(),
        pinned_at_ms: 1,
    };
    assert!(store.pin_host_key("prod", original).expect("first pin"));
    assert!(
        !store
            .pin_host_key(
                "prod",
                PinnedHostKey {
                    algorithm: "ssh-ed25519".into(),
                    fingerprint: "SHA256:first".into(),
                    pinned_at_ms: 2,
                },
            )
            .expect("same key")
    );
    assert!(matches!(
        store.pin_host_key(
            "prod",
            PinnedHostKey {
                algorithm: "ssh-ed25519".into(),
                fingerprint: "SHA256:changed".into(),
                pinned_at_ms: 3,
            },
        ),
        Err(SshError::SshHostKeyChanged { expected, actual })
            if expected == "SHA256:first" && actual == "SHA256:changed"
    ));
}

#[test]
fn command_cwd_quoting_cannot_inject_outside_the_cd_argument() {
    assert_eq!(
        super::runtime::command_in_cwd("pwd", Some("/tmp/a'b")),
        "cd -- '/tmp/a'\\''b' && pwd"
    );
}

#[test]
fn remote_stdout_and_stderr_share_the_existing_shell_output_cap() {
    let cap = haider_tools::PROCESS_MAX_OUTPUT_BYTES;
    let mut stdout = vec![b'o'; cap.saturating_sub(2)];
    let stderr = [b'e'];
    let mut truncated = false;
    let (limit_reached, retained) =
        super::runtime::append_bounded(&mut stdout, stderr.len(), b"xy", &mut truncated);
    assert!(limit_reached);
    assert_eq!(retained, 1);
    assert!(truncated);
    assert_eq!(stdout.len().saturating_add(stderr.len()), cap);
    assert_eq!(stdout.last(), Some(&b'x'));
}
