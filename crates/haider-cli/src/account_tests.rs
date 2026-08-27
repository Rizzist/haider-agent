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
