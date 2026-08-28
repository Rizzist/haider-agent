#![allow(clippy::expect_used)]
//! W-CFG — the headless session-config door and model-library enumeration:
//! flag vocabulary, feature preconditions, and the provider/model selector.
#![allow(dead_code)]

#[path = "../src/main.rs"]
mod cli_main;

use cli_main::models::{auth_state, availability_name};
use cli_main::session_config::{ConfigError, ConfigOptions, parse_options, resolve_model_selector};
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_rpc::{ProviderApiFamilyWire, ProviderAvailabilityWire, ProviderSummaryWire};

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|value| (*value).to_owned()).collect()
}

fn provider_summary(provider: &str) -> ProviderSummaryWire {
    ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: None,
        response_open_timeout_ms: None,
        models: vec![],
        model_details: vec![],
        inventory_fetched_at_ms: None,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
        auth_methods: vec![],
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: None,
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

/// MUTATION CHECK: let setters stack duplicates, accept a flag-shaped
/// value, or mis-map `--speed`. Expected RUNTIME failure: one row of this
/// vocabulary table flips.
#[test]
fn config_flag_vocabulary_is_exact() {
    let options = parse_options(&args(&[
        "--json",
        "--model",
        "gpt-5",
        "--effort",
        "high",
        "--speed",
        "fast",
        "--account",
        "work",
    ]))
    .expect("parses")
    .expect("not help");
    assert!(options.json);
    assert_eq!(options.model.as_deref(), Some("gpt-5"));
    assert_eq!(options.effort.as_deref(), Some("high"));
    assert_eq!(options.fast, Some(true));
    assert_eq!(options.account.as_deref(), Some("work"));
    assert!(options.mutates());

    assert!(
        !parse_options(&args(&["--json"]))
            .expect("parses")
            .expect("not help")
            .mutates()
    );
    assert_eq!(
        parse_options(&args(&["--speed", "warp"])),
        Err("--speed requires fast|normal".to_owned())
    );
    assert_eq!(
        parse_options(&args(&["--model", "--json"])),
        Err("--model requires a model id".to_owned()),
        "a flag-shaped value never becomes a model id"
    );
    assert_eq!(
        parse_options(&args(&["--model", "a", "--model", "b"])),
        Err("duplicate --model flag".to_owned())
    );
    assert!(parse_options(&args(&["--help"])).expect("parses").is_none());
}

/// MUTATION CHECK: drop a setter's feature precondition. Expected RUNTIME
/// failure: the required set loses the feature the daemon must serve.
#[test]
fn setter_feature_preconditions_are_per_flag() {
    let read_only = parse_options(&args(&["--json"]))
        .expect("parses")
        .expect("not help");
    let base = read_only.required_features();
    assert!(base.contains(haider_rpc::FEATURE_SESSION_CONFIG_V1));
    assert!(base.contains(haider_rpc::FEATURE_SESSION_OBSERVE_V1));
    assert!(!base.contains(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1));

    let with_model = ConfigOptions {
        model: Some("gpt-5".into()),
        ..Default::default()
    }
    .required_features();
    assert!(with_model.contains(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1));
    let with_speed = ConfigOptions {
        fast: Some(true),
        ..Default::default()
    }
    .required_features();
    assert!(with_speed.contains(haider_rpc::FEATURE_SESSION_FAST_SELECT_V1));

    let with_account = ConfigOptions {
        account: Some("work".into()),
        ..Default::default()
    }
    .required_features();
    assert_eq!(
        with_account, base,
        "an unimplemented account selector must not invent a daemon feature"
    );
    assert!(
        ConfigError::AccountSelectionUnsupported
            .to_string()
            .contains("--model provider/model")
    );
}

/// MUTATION CHECK: stop requiring a REGISTERED provider prefix, or accept
/// an empty model after the slash. Expected RUNTIME failure: the selector
/// resolves a pair the registry never named.
#[test]
fn model_selector_splits_only_on_registered_providers() {
    let providers = vec![provider_summary("openai"), provider_summary("xai")];
    assert_eq!(
        resolve_model_selector("openai/gpt-5", &providers).expect("resolves"),
        (Some("openai".to_owned()), "gpt-5".to_owned())
    );
    // An unregistered prefix is part of the MODEL name, not a provider.
    assert_eq!(
        resolve_model_selector("acme/gpt-5", &providers).expect("resolves"),
        (None, "acme/gpt-5".to_owned())
    );
    assert!(resolve_model_selector("openai/", &providers).is_err());
}

/// MUTATION CHECK: collapse the availability or auth-state vocabulary.
/// Expected RUNTIME failure: a JSON consumer's sniffed string changes.
#[test]
fn models_json_vocabulary_is_stable() {
    assert_eq!(
        availability_name(ProviderAvailabilityWire::Available),
        "available"
    );
    assert_eq!(
        availability_name(ProviderAvailabilityWire::Unavailable),
        "unavailable"
    );

    let mut provider = provider_summary("xai");
    provider.auth_methods = vec![AuthMethod::ApiKey];
    let active = CredentialDescriptor {
        alias: CredentialAlias::new("xai-main"),
        provider: "xai".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "key".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    assert_eq!(
        auth_state(&provider, std::slice::from_ref(&active)),
        "authenticated"
    );
    let mut inactive = active;
    inactive.active = false;
    assert_eq!(auth_state(&provider, &[inactive]), "inactive");
    assert_eq!(auth_state(&provider, &[]), "missing");
    provider.auth_methods = vec![];
    assert_eq!(auth_state(&provider, &[]), "not_required");
}

/// rev933b finding 5 MUTATION CHECK: drop the --confirm-epoch flag or stop
/// threading it into the selects. Expected RUNTIME failure: the parse row
/// flips (the threading half is pinned by the daemon's confirm law).
#[test]
fn confirm_epoch_flag_parses_once_and_defaults_off() {
    let options = parse_options(&args(&["--effort", "high", "--confirm-epoch"]))
        .expect("parses")
        .expect("not help");
    assert!(options.confirm_epoch);
    assert!(
        !parse_options(&args(&["--json"]))
            .expect("parses")
            .expect("not help")
            .confirm_epoch,
        "consent is never implied"
    );
    assert_eq!(
        parse_options(&args(&["--confirm-epoch", "--confirm-epoch"])),
        Err("duplicate --confirm-epoch flag".to_owned())
    );
}

/// rev933b finding 6 MUTATION CHECK: stop disclosing committed setters on a
/// mid-sequence failure. Expected RUNTIME failure: the rendered sentence
/// loses the committed list or the remedy.
#[test]
fn partial_failure_discloses_what_committed() {
    let error = cli_main::session_config::ConfigError::Partial {
        applied: vec!["model"],
        error: Box::new(cli_main::session_config::ConfigError::Protocol(
            "session.select_effort response method mismatch",
        )),
    };
    let rendered = error.to_string();
    assert!(rendered.contains("PARTIALLY applied"));
    assert!(rendered.contains("committed: model"));
    assert!(rendered.contains("session.select_effort"));
    assert!(rendered.contains("config --json"));
}
