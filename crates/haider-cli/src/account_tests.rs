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

#[test]
fn ambiguous_email_bridge_has_review_hint_without_false_removal_advice() {
    let view = |alias: &str, account_id: Option<&str>| AccountView {
        alias: alias.to_owned(),
        provider: "anthropic-oauth".to_owned(),
        auth_kind: "oauth",
        identity: Some(haider_protocol::credential::AccountIdentity {
            email: Some("OWNER@example.test".to_owned()),
            display_name: None,
            account_id: account_id.map(str::to_owned),
            plan: None,
            issuer: None,
            captured_at: 1,
            verified: false,
        }),
        created: None,
        active: false,
        usable: true,
    };
    let document = AccountsDocument {
        schema: ACCOUNTS_SCHEMA,
        accounts: vec![
            view("legacy", None),
            view("member-a", Some("account=a;organization=team")),
            view("member-b", Some("account=b;organization=team")),
        ],
    };
    let text = render_human(&document);
    assert!(text.contains("email matches multiple account IDs"));
    assert!(!text.contains("remove this duplicate"));

    let document = AccountsDocument {
        schema: ACCOUNTS_SCHEMA,
        accounts: vec![
            view("legacy", None),
            view("member-a", Some("account=a;organization=team")),
        ],
    };
    let text = render_human(&document);
    assert!(text.contains("remove this duplicate"));
}

fn claude_view(alias: &str, account_id: &str, active: bool, usable: bool) -> AccountView {
    AccountView {
        alias: alias.to_owned(),
        provider: "anthropic-oauth".to_owned(),
        auth_kind: "oauth",
        identity: Some(haider_protocol::credential::AccountIdentity {
            email: None,
            display_name: Some("Claude".to_owned()),
            account_id: Some(account_id.to_owned()),
            plan: None,
            issuer: None,
            captured_at: 1,
            verified: false,
        }),
        created: None,
        active,
        usable,
    }
}

#[test]
fn duplicate_hint_keeps_the_active_then_live_row() {
    let same = "account=a;organization=o";
    // The first-listed row is dead; the just-signed-in active row is later.
    let document = AccountsDocument {
        schema: ACCOUNTS_SCHEMA,
        accounts: vec![
            claude_view("old-dead", same, false, false),
            claude_view("fresh", same, true, true),
        ],
    };
    let text = render_human(&document);
    assert!(text.contains(
        "matching identity already kept as `fresh`; remove this duplicate explicitly with `haider account remove old-dead --confirm`"
    ));
    assert!(!text.contains("haider account remove fresh"), "{text}");

    // Without an active row, a usable one is kept over a dead one.
    let document = AccountsDocument {
        schema: ACCOUNTS_SCHEMA,
        accounts: vec![
            claude_view("old-dead", same, false, false),
            claude_view("live", same, false, true),
        ],
    };
    let text = render_human(&document);
    assert!(text.contains("already kept as `live`"));
    assert!(!text.contains("haider account remove live"), "{text}");
    // JSON output stays exactly the documented schema.
    let json = serde_json::to_value(&document).expect("json");
    assert!(json["accounts"][0].get("active").is_none());
    assert!(json["accounts"][0].get("usable").is_none());
}

#[test]
fn unverified_claude_rows_are_flagged_as_suspected_duplicates() {
    let document = AccountsDocument {
        schema: ACCOUNTS_SCHEMA,
        accounts: vec![
            claude_view(
                "anthropic-oauth-2",
                "anthropic:0123456789abcdef",
                false,
                false,
            ),
            claude_view("anthropic-oauth-3", "account=a;organization=o", true, true),
        ],
    };
    let text = render_human(&document);
    assert!(text.contains(
        "identity unverified; suspected duplicate of `anthropic-oauth-3`; re-login it in place from `/accounts` (enter twice on the row) to verify it, or remove it explicitly with `haider account remove anthropic-oauth-2 --confirm`"
    ), "{text}");
    assert!(
        !text.contains("haider account remove anthropic-oauth-3"),
        "{text}"
    );
    let alone = AccountsDocument {
        schema: ACCOUNTS_SCHEMA,
        accounts: vec![claude_view(
            "anthropic-oauth",
            "anthropic:0123456789abcdef",
            true,
            true,
        )],
    };
    assert!(
        render_human(&alone)
            .contains("identity unverified; it is verified automatically while its token works")
    );
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
async fn account_use_cache_warning_names_the_exact_confirmation_command() {
    let client = FakeAccountClient::new([
        list_response("work", Some(3)),
        ResponseBody::Error {
            code: haider_rpc::ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED.to_owned(),
            message: "cache epoch change: account".to_owned(),
            retryable: false,
            data: None,
        },
    ]);
    let error = execute_use(&client, "work".to_owned(), false)
        .await
        .expect_err("selection must require confirmation");

    assert!(
        error
            .to_string()
            .contains("run `haider account use work --confirm`")
    );
    let hint = error.to_string();
    let instruction = hint
        .split('`')
        .find(|part| part.starts_with("haider account "))
        .expect("printed command");
    let words = instruction
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let command = parse_account_command(&words[2..]).expect("literal selection hint");
    let client = FakeAccountClient::new([
        list_response("work", Some(3)),
        ResponseBody::AccountSetActive {
            descriptor: descriptor("work"),
            prior_alias: None,
            revision: 4,
        },
    ]);
    assert!(matches!(execute(&client, command).await, Ok(code) if code == ExitCode::SUCCESS));
    assert!(matches!(
        client.requests().as_slice(),
        [
            RequestBody::AccountList { .. },
            RequestBody::AccountSetActive {
                confirm_new_epoch: true,
                ..
            }
        ]
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

/// Execute the entire advertised command literally through the CLI parser and
/// request dispatcher. Unsupported rows must never advertise a confirmation.
#[tokio::test]
async fn every_printed_account_suggestion_runs_as_written() {
    let supported = codex_candidate();
    let instruction = import_confirmation_instruction(&supported).expect("supported instruction");
    let words = instruction
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(&words[..2], &["haider", "account"]);
    let command = parse_account_command(&words[2..]).expect("literal suggestion parses");
    let client = FakeAccountClient::new([
        ResponseBody::AccountDeviceCandidates {
            discovery_disabled: false,
            candidates: vec![supported],
            adoption_available: Vec::new(),
        },
        ResponseBody::AccountImportDevice {
            descriptor: descriptor("imported"),
            revision: 2,
        },
    ]);
    assert!(matches!(execute(&client, command).await, Ok(code) if code == ExitCode::SUCCESS));
    assert!(matches!(
        client.requests().as_slice(),
        [
            RequestBody::AccountDeviceCandidates,
            RequestBody::AccountImportDevice { .. }
        ]
    ));

    for source in ["codex", "claude-code"] {
        let mut unsupported = codex_candidate();
        unsupported.source = source.into();
        unsupported.import_supported = false;
        unsupported.unsupported_reason = Some("synthetic unsupported source".into());
        assert_eq!(import_confirmation_instruction(&unsupported), None);
        let client = FakeAccountClient::new([ResponseBody::AccountDeviceCandidates {
            discovery_disabled: false,
            candidates: vec![unsupported],
            adoption_available: Vec::new(),
        }]);
        assert!(
            matches!(execute(&client, AccountCommand::Import { source: source.into(), confirm: false }).await,
            Ok(code) if code == ExitCode::from(EX_USAGE))
        );
        assert!(matches!(
            client.requests().as_slice(),
            [RequestBody::AccountDeviceCandidates]
        ));
    }
    for old_id in ["account=a;organization=o", "anthropic:0123456789abcdef"] {
        let document = AccountsDocument {
            schema: ACCOUNTS_SCHEMA,
            accounts: vec![
                claude_view("old-dead", old_id, false, false),
                claude_view("fresh", "account=a;organization=o", true, true),
            ],
        };
        let printed = render_human(&document);
        let suggestions = printed
            .split('`')
            .filter(|part| part.starts_with("haider account "))
            .collect::<Vec<_>>();
        assert_eq!(
            suggestions.len(),
            1,
            "each duplicate variant advertises one executable command"
        );
        for suggestion in suggestions {
            let words = suggestion
                .split_ascii_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let command = parse_account_command(&words[2..]).expect("literal removal hint");
            let client = FakeAccountClient::new([
                list_response("old-dead", Some(1)),
                ResponseBody::AccountRemove {
                    removed_alias: CredentialAlias::new("old-dead"),
                    replacement_active_alias: None,
                    revision: 2,
                },
            ]);
            assert!(
                matches!(execute(&client, command).await, Ok(code) if code == ExitCode::SUCCESS)
            );
            assert!(
                matches!(client.requests().as_slice(), [RequestBody::AccountList { .. }, RequestBody::AccountRemove { alias, expected_revision: Some(1), .. }] if alias == "old-dead")
            );
        }
    }
}

#[tokio::test]
async fn explicit_unsupported_claude_import_reaches_live_and_fake_daemon_policy() {
    for reason in [
        "owned by the official client",
        "fake OAuth mode refuses Claude Code import",
    ] {
        let mut candidate = codex_candidate();
        candidate.source = "claude-code".into();
        candidate.import_supported = false;
        candidate.unsupported_reason = Some("owned by the official client".into());
        let client = FakeAccountClient::new([
            ResponseBody::AccountDeviceCandidates {
                discovery_disabled: false,
                candidates: vec![candidate],
                adoption_available: Vec::new(),
            },
            ResponseBody::Error {
                code: "invalid_argument".into(),
                message: reason.into(),
                retryable: false,
                data: None,
            },
        ]);
        let result = execute(
            &client,
            AccountCommand::Import {
                source: "claude-code".into(),
                confirm: true,
            },
        )
        .await;
        assert!(matches!(result, Err(AccountError::Rpc { message, .. }) if message == reason));
        assert!(matches!(
            client.requests().as_slice(),
            [
                RequestBody::AccountDeviceCandidates,
                RequestBody::AccountImportDevice { .. }
            ]
        ));
    }
}
