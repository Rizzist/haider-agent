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
