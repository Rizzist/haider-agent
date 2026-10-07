#![allow(dead_code, unexpected_cfgs)]
fn o17_store_root(name: &str) -> std::path::PathBuf {
    let root = std::path::PathBuf::from(std::env::var("ASTRA15_SCRATCH").unwrap())
        .join(format!("o17-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("work")).unwrap();
    root
}

fn o17_create(
    id: &str,
    cwd: &str,
    provider: &str,
    model: &str,
    body: &serde_json::Value,
) -> haider_store::SessionCreateCommand {
    use haider_protocol::ids::{DeviceId, EventId, SessionId};
    let json = body.to_string();
    haider_store::SessionCreateCommand {
        command_id: format!("delegation-session-{id}"),
        request_digest: blake3::hash(json.as_bytes()).to_hex().to_string(),
        request_json: json,
        session_id: SessionId::new(id),
        cwd: cwd.into(),
        provider: provider.into(),
        model: model.into(),
        max_tokens: 4096,
        max_tokens_source: None,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test".into(),
        event_id: EventId::new(format!("created-{id}")),
        device_id: DeviceId::new("test"),
    }
}

fn o17_old_body(cwd: &str, provider: &str, model: &str) -> serde_json::Value {
    serde_json::json!({"cwd":cwd, "provider":provider, "model":model, "max_tokens":4096,
        "permission_overrides":null, "delegation_agent":"agent-o17", "effort":null, "fast":false,
        "cache_policy":haider_protocol::cache::CachePolicySettingsV1::default(), "interaction_mode":"interactive"})
}

/// The body the CANDIDATE delegation.rs:767-792 builds on replay (15 keys).
fn o17_new_body(
    old: &serde_json::Value,
    parent: &str,
    meta: &haider_protocol::session::SessionMetadataV1,
) -> serde_json::Value {
    let mut new = old.clone();
    new["max_tokens_source"] = serde_json::to_value(
        haider_protocol::output_budget::SessionOutputBudgetSourceV1::classify(None, 4096),
    )
    .unwrap();
    new["inheritance_parent_session_id"] = serde_json::json!(parent);
    new["inherited_account_alias"] = serde_json::json!(meta.account_alias);
    new["inherited_provider_base_url"] = serde_json::json!(meta.provider_base_url);
    new["inherited_provider_rebind_id"] = serde_json::json!(meta.provider_rebind_id);
    new
}

fn o17_replay(
    store: &haider_store::Store,
    id: &str,
    body: &serde_json::Value,
) -> Result<bool, String> {
    let json = body.to_string();
    store
        .session_create_receipt(
            &format!("delegation-session-{id}"),
            blake3::hash(json.as_bytes()).to_hex().as_ref(),
            &json,
        )
        .map(|r| r.is_some())
        .map_err(|e| format!("{e:?}"))
}

fn o17_rebind(
    store: &haider_store::Store,
    session: &str,
    n: u32,
    provider: &str,
    base: Option<&str>,
    account: Option<&str>,
) {
    use haider_protocol::ids::{DeviceId, EventId, SessionId};
    store
        .rebind_session_provider(&haider_store::SessionProviderRebindCommand {
            command_id: format!("o17-rebind-{session}-{n}"),
            request_digest: format!("o17-rebind-digest-{session}-{n}"),
            request_json: "{}".into(),
            session_id: SessionId::new(session),
            worker_generation: store.worker_generation(),
            provider: provider.into(),
            base_url: base.map(str::to_owned),
            account: account.map(str::to_owned),
            event_id: EventId::new(format!("o17-rebound-{session}-{n}")),
            device_id: DeviceId::new("test"),
        })
        .expect("rebind");
}

const CASES: [(&str, bool, bool, bool, bool, bool); 6] = [
    ("creation-cross", true, false, false, false, false),
    ("creation-roundtrip", true, false, true, false, false),
    ("rebind", false, true, false, false, false),
    ("unpinned", false, false, false, false, false),
    ("other-child", true, false, false, true, false),
    ("later-rebind", true, false, false, false, true),
];
fn pick(
    store: &haider_store::Store,
    parent: &haider_store::SessionCreateCommand,
    n: usize,
    provider: &str,
    model: &str,
) {
    use haider_protocol::ids::{DeviceId, EventId};
    store
        .select_session_model(&haider_store::SessionSelectModelCommand {
            command_id: format!("mixed-pick-{n}"),
            request_digest: format!("mixed-digest-{n}"),
            request_json: "{}".into(),
            session_id: parent.session_id.clone(),
            worker_generation: store.worker_generation(),
            provider: provider.into(),
            model: model.into(),
            expected_pair: None,
            #[cfg(not(old_writer))]
            account_alias: None,
            output_budget: None,
            event_id: EventId::new(format!("mixed-picked-{n}")),
            device_id: DeviceId::new("test"),
        })
        .unwrap();
}
fn create_parent(
    root: &std::path::Path,
    creation_pin: bool,
    rebind_pin: bool,
    roundtrip: bool,
    rebound_later: bool,
) {
    std::fs::create_dir_all(root.join("work")).unwrap();
    let cwd = root.join("work").to_str().unwrap().to_owned();
    let store = haider_store::Store::open(root).unwrap();
    let parent = o17_create(
        "mixed-parent",
        &cwd,
        "bedrock",
        "anthropic.claude-opus-5",
        &serde_json::json!({"parent":true}),
    );
    store
        .create_session_with_configuration(
            &parent,
            haider_protocol::session::SessionInteractionModeV1::Interactive,
            creation_pin.then(|| "bed-b".into()),
        )
        .unwrap();
    if rebind_pin {
        o17_rebind(
            &store,
            "mixed-parent",
            1,
            "bedrock",
            Some("http://127.0.0.1:9"),
            Some("bed-b"),
        );
    }
    pick(&store, &parent, 0, "anthropic", "claude-sonnet-5");
    if roundtrip {
        pick(&store, &parent, 1, "bedrock", "anthropic.claude-opus-5");
    }
    let meta = store.session_metadata(&parent.session_id).unwrap().unwrap();
    #[cfg(not(old_writer))]
    assert_eq!(meta.account_alias, None);
    if rebound_later {
        o17_rebind(
            &store,
            "mixed-parent",
            2,
            &meta.provider,
            None,
            Some("new-pin"),
        );
    }
    println!(
        "created parent {} {:?}",
        root.display(),
        store
            .session_metadata(&parent.session_id)
            .unwrap()
            .unwrap()
            .account_alias
    );
}
fn child(root: &std::path::Path, other_child: bool) {
    let store = haider_store::Store::open(root).unwrap();
    let cwd = root.join("work").to_str().unwrap().to_owned();
    let meta = store
        .session_metadata(&haider_protocol::ids::SessionId::new("mixed-parent"))
        .unwrap()
        .unwrap();
    let (provider, model) = if other_child {
        ("openai", "gpt-5")
    } else {
        (meta.provider.as_str(), meta.model.as_str())
    };
    let old = o17_old_body(&cwd, provider, model);
    assert_eq!(old.as_object().unwrap().len(), 10);
    store
        .create_session(&o17_create("mixed-child", &cwd, provider, model, &old))
        .unwrap();
    assert_eq!(o17_replay(&store, "mixed-child", &old), Ok(true));
    std::fs::write(root.join("old-body.json"), old.to_string()).unwrap();
    println!(
        "old writer created child {} parent_alias={:?}",
        root.display(),
        meta.account_alias
    );
}
fn replay(root: &std::path::Path, other_child: bool) -> bool {
    let store = haider_store::Store::open(root).unwrap();
    let old: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("old-body.json")).unwrap())
            .unwrap();
    assert_eq!(o17_replay(&store, "mixed-child", &old), Ok(true));
    let mut meta = store
        .session_metadata(&haider_protocol::ids::SessionId::new("mixed-parent"))
        .unwrap()
        .unwrap();
    if other_child {
        meta.account_alias = None;
        meta.provider_base_url = None;
        meta.provider_rebind_id = None;
    }
    let correct = o17_new_body(&old, "mixed-parent", &meta);
    let mut wrong = correct.clone();
    wrong["inherited_account_alias"] =
        serde_json::json!(if meta.account_alias.as_deref() == Some("bed-b") {
            "invented"
        } else {
            "bed-b"
        });
    let good = o17_replay(&store, "mixed-child", &correct);
    let bad = o17_replay(&store, "mixed-child", &wrong);
    let parent_events = store
        .journal_replay(&haider_protocol::ids::SessionId::new("mixed-parent"))
        .unwrap();
    println!(
        "{}",
        serde_json::json!({"case":root.file_name().unwrap().to_str().unwrap(),"correct_body":correct,"correct_result":format!("{good:?}"),"invented_pin_result":format!("{bad:?}"),"pass":good==Ok(true)&&bad.is_err(),"parent_events":parent_events})
    );
    good == Ok(true) && bad.is_err()
}
fn prepare_metadata(root: &std::path::Path) {
    let store = haider_store::Store::open(root).unwrap();
    // Pre-epoch metadata is projected lazily by this getter, not Store::open.
    // Prepare it before measuring whether receipt preflight changes anything.
    for id in ["mixed-parent", "mixed-child"] {
        store
            .session_metadata(&haider_protocol::ids::SessionId::new(id))
            .unwrap()
            .unwrap();
    }
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let stage = &args[1];
    let root = std::path::PathBuf::from(&args[2]);
    match stage.as_str() {
        "seed" => {
            for (name, pin, rebind, roundtrip, _other, later) in CASES {
                create_parent(&root.join(name), pin, rebind, roundtrip, later);
            }
        }
        "old-write" => {
            for (name, _, _, _, other, _) in CASES {
                child(&root.join(name), other);
            }
            create_parent(&root.join("true-old-kept-pin"), true, false, true, false);
            child(&root.join("true-old-kept-pin"), false);
        }
        "candidate-open" => {
            for (name, _, _, _, _, _) in CASES {
                prepare_metadata(&root.join(name));
            }
            prepare_metadata(&root.join("true-old-kept-pin"));
        }
        "old-parent-open" => {
            prepare_metadata(&root.join("old-created-parent"));
        }
        "old-parent-seed" => {
            let root = root.join("old-created-parent");
            std::fs::create_dir_all(root.join("work")).unwrap();
            let cwd = root.join("work").to_str().unwrap().to_owned();
            let store = haider_store::Store::open(&root).unwrap();
            let parent = o17_create(
                "mixed-parent",
                &cwd,
                "bedrock",
                "anthropic.claude-opus-5",
                &serde_json::json!({"parent":true}),
            );
            store
                .create_session_with_configuration(
                    &parent,
                    haider_protocol::session::SessionInteractionModeV1::Interactive,
                    Some("bed-b".into()),
                )
                .unwrap();
        }
        "old-parent-clear" => {
            let root = root.join("old-created-parent");
            let cwd = root.join("work").to_str().unwrap().to_owned();
            let store = haider_store::Store::open(&root).unwrap();
            let parent = o17_create(
                "mixed-parent",
                &cwd,
                "bedrock",
                "anthropic.claude-opus-5",
                &serde_json::json!({"parent":true}),
            );
            pick(&store, &parent, 0, "anthropic", "claude-sonnet-5");
            pick(&store, &parent, 1, "bedrock", "anthropic.claude-opus-5");
            assert_eq!(
                store
                    .session_metadata(&parent.session_id)
                    .unwrap()
                    .unwrap()
                    .account_alias,
                None
            );
        }
        "old-parent-write" => child(&root.join("old-created-parent"), false),
        "old-parent-replay" => {
            assert!(replay(&root.join("old-created-parent"), false));
        }
        "replay" => {
            let mut failures = 0;
            for (name, _, _, _, other, _) in CASES {
                if !replay(&root.join(name), other) {
                    failures += 1;
                }
            }
            if !replay(&root.join("true-old-kept-pin"), false) {
                failures += 1;
            }
            println!("TOTAL 7 cases, {failures} failures");
            std::process::exit(if failures > 0 { 1 } else { 0 });
        }
        _ => panic!("unknown stage"),
    }
}
