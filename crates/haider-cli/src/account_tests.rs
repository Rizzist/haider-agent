#![allow(clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::Mutex;

use haider_protocol::credential::CredentialStatus;
use haider_protocol::ids::CredentialAlias;

use super::*;

struct FakeAccountClient {
    requests: Mutex<Vec<RequestBody>>,
    responses: Mutex<VecDeque<ResponseBody>>,
}

impl FakeAccountClient {
    fn new(responses: impl IntoIterator<Item = ResponseBody>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    fn requests(&self) -> Vec<RequestBody> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl AccountClient for FakeAccountClient {
    fn request(
        &self,
        request: RequestBody,
    ) -> impl Future<Output = Result<ResponseBody, ClientError>> + Send {
        self.requests.lock().expect("request lock").push(request);
        let response = self
            .responses
            .lock()
            .expect("response lock")
            .pop_front()
            .expect("fake response");
        std::future::ready(Ok(response))
    }
}

fn descriptor(alias: &str) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: "anthropic".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "fixture".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}

fn list_response(alias: &str, revision: Option<u64>) -> ResponseBody {
    ResponseBody::AccountList {
        descriptors: vec![descriptor(alias)],
        revision,
        provider_active: Vec::new(),
        provider_defaults: Vec::new(),
        sources: Vec::new(),
        availability: Some(SnapshotAvailabilityWire::Available),
    }
}

#[test]
fn account_list_projection_is_additive_and_preserves_legacy_absence() {
    let legacy = account_view(descriptor("legacy"));
    assert_eq!(legacy.identity, None);
    assert_eq!(legacy.created, None);

    let mut current = descriptor("current");
    current.account_identity = codex_candidate().identity;
    current.created_at_ms = Some(964);
    let projected = account_view(current.clone());
    assert_eq!(projected.identity, current.account_identity);
    assert_eq!(projected.created, Some(964));
}

fn codex_candidate() -> haider_rpc::DeviceCredentialCandidateWire {
    haider_rpc::DeviceCredentialCandidateWire {
        candidate: format!("dc1_{}", "0".repeat(64)),
        source: "codex".into(),
        provider: "openai-oauth".into(),
        source_label: "Codex".into(),
        account_label: Some("owner@example.invalid".into()),
        identity: Some(haider_protocol::credential::AccountIdentity {
            email: Some("owner@example.invalid".into()),
            display_name: None,
            account_id: Some("acct-964".into()),
            plan: Some("pro".into()),
            issuer: Some("https://auth.openai.com".into()),
            captured_at: 964,
            verified: false,
        }),
        freshness: "fresh".into(),
        expires_at_ms: None,
        path: "/home/test/.codex/auth.json".into(),
        import_supported: true,
        unsupported_reason: None,
    }
}

#[test]
fn import_and_refresh_parsing_are_explicit() {
    assert_eq!(
        parse_account_command(&["import".into(), "codex".into()]),
        Ok(AccountCommand::Import {
            source: "codex".into(),
            confirm: false,
        })
    );
    assert_eq!(
        parse_account_command(&["import".into(), "codex".into(), "--confirm".into()]),
        Ok(AccountCommand::Import {
            source: "codex".into(),
            confirm: true,
        })
    );
    assert_eq!(
        parse_account_command(&["refresh".into(), "work".into()]),
        Ok(AccountCommand::Refresh {
            alias: "work".into(),
        })
    );
}

#[test]
fn source_and_profile_default_commands_have_exact_grammar() {
    assert_eq!(
        parse_account_command(&["use".into(), "work".into()]),
        Ok(AccountCommand::Use {
            alias: "work".into(),
            confirm: false,
        })
    );
    assert_eq!(
        parse_account_command(&["use".into(), "work".into(), "--confirm".into()]),
        Ok(AccountCommand::Use {
            alias: "work".into(),
            confirm: true,
        })
    );
    assert_eq!(
        parse_account_command(&[
            "source".into(),
            "add".into(),
            "codex".into(),
            "/tmp/codex-work".into(),
            "--label".into(),
            "Work".into(),
        ]),
        Ok(AccountCommand::SourceAdd {
            kind: "codex".into(),
            root: "/tmp/codex-work".into(),
            label: Some("Work".into()),
        })
    );
    assert_eq!(
        parse_account_command(&["source".into(), "scan".into(), "--json".into()]),
        Ok(AccountCommand::SourceScan { json: true })
    );
    assert!(
        parse_account_command(&["source".into(), "add".into(), "keychain".into(), "/".into()])
            .is_err()
    );
}

/// LAW (970 layer B): `source add` accepts every enrolled kind's durable
/// name and its short alias, and nothing else. The usage line names the
/// same vocabulary the daemon parses.
#[test]
fn source_add_accepts_every_enrolled_source_kind_vocabulary() {
    for kind in [
        "codex",
        "codex_home",
        "claude",
        "claude_file",
        "grok",
        "grok_home",
        "kimi",
        "kimi_code_home",
    ] {
        assert_eq!(
            parse_account_command(&[
                "source".into(),
                "add".into(),
                kind.into(),
                "/tmp/root".into()
            ]),
            Ok(AccountCommand::SourceAdd {
                kind: kind.into(),
                root: "/tmp/root".into(),
                label: None,
            }),
            "{kind}"
        );
        assert_eq!(
            parse_account_command(&[
                "source".into(),
                "add".into(),
                kind.into(),
                "/tmp/root".into(),
                "--label".into(),
                "Origin".into(),
            ]),
            Ok(AccountCommand::SourceAdd {
                kind: kind.into(),
                root: "/tmp/root".into(),
                label: Some("Origin".into()),
            }),
            "{kind} with a label"
        );
    }
    for rejected in ["grok_cli", "kimi-code", "keyring", "gemini"] {
        assert!(
            parse_account_command(&[
                "source".into(),
                "add".into(),
                rejected.into(),
                "/tmp/root".into(),
            ])
            .is_err(),
            "{rejected}"
        );
    }
    let usage = account_usage();
    for kind in ["codex", "claude_file", "grok", "kimi_code_home"] {
        assert!(usage.contains(kind), "usage names {kind}");
    }
}

#[tokio::test]
async fn account_use_resolves_alias_then_sets_the_profile_default() {
    let selected = descriptor("work");
    let client = FakeAccountClient::new([
        list_response("work", Some(3)),
        ResponseBody::AccountSetActive {
            descriptor: selected,
            prior_alias: None,
            revision: 4,
        },
    ]);
    let result = execute(
        &client,
        AccountCommand::Use {
            alias: "work".into(),
            confirm: true,
        },
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
    assert!(matches!(
        client.requests().as_slice(),
        [
            RequestBody::AccountList { provider: None },
            RequestBody::AccountSetActive {
                alias,
                confirm_new_epoch: true,
                ..
            }
        ] if alias == "work"
    ));
}

#[tokio::test]
async fn source_add_sends_coordinates_without_credential_material() {
    let source = AccountSourceWire {
        source_id: format!("src1_{}", "1".repeat(64)),
        account_alias: None,
        kind: "codex_home".into(),
        label: "Work".into(),
        path: Some("/tmp/codex-work".into()),
        credential_store: "file".into(),
        refresh_owner: "codex".into(),
        health: "ready".into(),
        last_seen_at_ms: None,
        last_refreshed_at_ms: None,
        access_expires_at_ms: None,
        plan: None,
        masked_identity: None,
    };
    let client = FakeAccountClient::new([ResponseBody::AccountSourceAdd {
        source: source.clone(),
        sources: vec![source],
    }]);
    let result = execute(
        &client,
        AccountCommand::SourceAdd {
            kind: "codex".into(),
            root: "/tmp/codex-work".into(),
            label: Some("Work".into()),
        },
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
    assert!(matches!(
        client.requests().as_slice(),
        [RequestBody::AccountSourceAdd { kind, root, label: Some(label), .. }]
            if kind == "codex" && root == "/tmp/codex-work" && label == "Work"
    ));
}

#[tokio::test]
async fn unconfirmed_import_discovers_but_never_mutates() {
    let client = FakeAccountClient::new([ResponseBody::AccountDeviceCandidates {
        discovery_disabled: false,
        candidates: vec![codex_candidate()],
        adoption_available: Vec::new(),
    }]);
    let result = execute(
        &client,
        AccountCommand::Import {
            source: "codex".into(),
            confirm: false,
        },
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::from(EX_USAGE)));
    assert!(matches!(
        client.requests().as_slice(),
        [RequestBody::AccountDeviceCandidates]
    ));
}

#[tokio::test]
async fn confirmed_import_sends_only_the_opaque_candidate() {
    let mut imported = descriptor("codex-copy");
    imported.provider = "openai-oauth".into();
    imported.auth_method = AuthMethod::OAuth;
    imported.account_identity = codex_candidate().identity;
    let candidate = codex_candidate();
    let candidate_id = candidate.candidate.clone();
    let client = FakeAccountClient::new([
        ResponseBody::AccountDeviceCandidates {
            discovery_disabled: false,
            candidates: vec![candidate],
            adoption_available: Vec::new(),
        },
        ResponseBody::AccountImportDevice {
            descriptor: imported,
            revision: 2,
        },
    ]);
    let result = execute(
        &client,
        AccountCommand::Import {
            source: "codex".into(),
            confirm: true,
        },
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
    assert!(matches!(
        client.requests().as_slice(),
        [
            RequestBody::AccountDeviceCandidates,
            RequestBody::AccountImportDevice { candidate, .. }
        ] if candidate == &candidate_id
    ));
}

/// MUTATION CHECK: remove the account.list preflight or stop propagating
/// its revision. Expected RUNTIME failure: request count/order or the
/// exact `expected_revision` assertion changes.
#[tokio::test]
async fn confirmed_remove_is_list_first_and_revision_fenced() {
    let client = FakeAccountClient::new([
        list_response("probe", Some(41)),
        ResponseBody::AccountRemove {
            removed_alias: CredentialAlias::new("probe"),
            replacement_active_alias: None,
            revision: 42,
        },
    ]);
    let result = execute(
        &client,
        AccountCommand::Remove {
            alias: "probe".into(),
            confirm: true,
        },
    )
    .await;
    assert!(matches!(result, Ok(code) if code == ExitCode::SUCCESS));
    let requests = client.requests();
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        &requests[0],
        RequestBody::AccountList { provider: None }
    ));
    assert!(matches!(
        &requests[1],
        RequestBody::AccountRemove {
            alias,
            expected_revision: Some(41),
            ..
        } if alias == "probe"
    ));
}

#[tokio::test]
async fn confirmed_remove_refuses_to_mutate_without_a_snapshot_revision() {
    let client = FakeAccountClient::new([list_response("probe", None)]);
    let result = execute(
        &client,
        AccountCommand::Remove {
            alias: "probe".into(),
            confirm: true,
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(AccountError::Protocol(
            "account.list omitted the revision required for removal"
        ))
    ));
    assert!(matches!(
        client.requests().as_slice(),
        [RequestBody::AccountList { provider: None }]
    ));
}
