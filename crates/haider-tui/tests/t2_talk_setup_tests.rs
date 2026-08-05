//! T2 — the `/talk` setup card's reducer laws, driven with engine mocks
//! (TalkEvents): engine picker, whisper model rows with download
//! progress, the Deepgram key paste → validate → vault → models → select
//! → language flow, the reuse path, and the honest error states.
#![allow(clippy::expect_used)]

use haider_rpc::SecretWire;
use haider_stt::SttError;
use haider_stt::config::{TranscriptionConfig, TranscriptionEngine};
use haider_tui::app::{AppEvent, AppModel, AppRequest, RuntimeMode, Screen};
use haider_tui::talk::{
    DeepgramModelRow, KeyStage, RuntimeRowState, SetupStage, TalkEvent, TalkSetupSnapshot,
    TalkShellCommand, WhisperRowState,
};
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, submit};

fn sid() -> haider_protocol::ids::SessionId {
    haider_protocol::ids::SessionId::new("s-setup")
}

fn live_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned());
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    assert_eq!(model.screen, Screen::Session);
    model.requests.clear();
    model
}

fn snapshot(installed: &[&str], runtime: Option<&str>) -> TalkSetupSnapshot {
    TalkSetupSnapshot {
        config: Ok(TranscriptionConfig::default()),
        whisper_dir: Some("/tmp/DiffForge/whisper".to_owned()),
        installed: installed.iter().map(|id| (*id).to_owned()).collect(),
        selected_hint: Some("base.en".to_owned()),
        runtime: runtime.map(str::to_owned),
        runtime_hint: "brew install whisper-cpp".to_owned(),
    }
}

fn open_setup(model: &mut AppModel) {
    submit(model, "/talk setup");
    assert!(model.talk_setup.is_some(), "the card opened");
    model.requests.clear();
}

fn talk_shell(requests: &[AppRequest]) -> Vec<&TalkShellCommand> {
    requests
        .iter()
        .filter_map(|request| match request {
            AppRequest::TalkShell(command) => Some(command),
            _ => None,
        })
        .collect()
}

fn models() -> Vec<DeepgramModelRow> {
    vec![
        DeepgramModelRow {
            name: "nova-3".to_owned(),
            languages: "multi".to_owned(),
        },
        DeepgramModelRow {
            name: "nova-3-medical".to_owned(),
            languages: "en".to_owned(),
        },
    ]
}

/// MUTATION CHECK: drop the `LoadSetup`/`TranscriptionSecretRead` pushes
/// from `open_talk_setup`. Expected failure: the card opens permanently
/// "loading…" with no snapshot request and no key-presence read.
#[test]
fn slash_talk_setup_opens_and_loads_the_world() {
    let mut model = live_session();
    submit(&mut model, "/talk setup");
    let card = model.talk_setup.as_ref().expect("card open");
    assert_eq!(card.stage, SetupStage::Engine);
    assert!(!card.loaded);
    assert!(card.vault_supported);
    assert_eq!(
        talk_shell(&model.requests),
        vec![&TalkShellCommand::LoadSetup]
    );
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::TranscriptionSecretRead)),
        "key presence is daemon truth, read at open"
    );
}

/// MUTATION CHECK: build the whisper rows from `installed` alone (skip
/// the catalog). Expected failure: absent models have no row to download
/// from.
#[test]
fn the_snapshot_installs_catalog_rows_with_filesystem_truth() {
    let mut model = live_session();
    open_setup(&mut model);
    model.handle_talk(TalkEvent::SetupSnapshot {
        snapshot: snapshot(&["base.en"], Some("/opt/homebrew/bin/whisper-cli")),
    });
    let card = model.talk_setup.as_ref().expect("card open");
    assert!(card.loaded);
    let ids: Vec<&str> = card.whisper.iter().map(|row| row.id).collect();
    assert_eq!(ids, vec!["tiny.en", "base.en", "small.en"]);
    assert_eq!(card.whisper[0].state, WhisperRowState::Absent);
    assert_eq!(card.whisper[1].state, WhisperRowState::Installed);
    assert_eq!(card.whisper[2].state, WhisperRowState::Absent);
    assert_eq!(
        card.runtime,
        RuntimeRowState::Found("/opt/homebrew/bin/whisper-cli".to_owned())
    );
}

/// MUTATION CHECK: save the config optimistically (close the card at ⏎).
/// Expected failure: the card closes before `ConfigStored` and a failed
/// save vanishes silently.
#[test]
fn selecting_an_installed_model_saves_local_config_through_the_store() {
    let mut model = live_session();
    open_setup(&mut model);
    model.handle_talk(TalkEvent::SetupSnapshot {
        snapshot: snapshot(&["base.en"], Some("/opt/homebrew/bin/whisper-cli")),
    });
    // Engine → local.
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        model.talk_setup.as_ref().expect("open").stage,
        SetupStage::Local
    );
    // ↓ to base.en (row 1), ⏎ selects it.
    model.handle(key(KeyCode::Down));
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    let stored = talk_shell(&model.requests);
    let TalkShellCommand::StoreConfig { config } = stored[0] else {
        panic!("expected StoreConfig, got {stored:?}");
    };
    assert_eq!(config.engine, TranscriptionEngine::Local);
    assert_eq!(config.whisper_model_id.as_deref(), Some("base.en"));
    assert!(model.talk_setup.as_ref().expect("open").saving);
    assert!(model.talk_setup.is_some(), "no optimistic close");
    // The stored reply closes the card and installs the config.
    model.handle_talk(TalkEvent::ConfigStored {
        config: config.clone(),
        error: None,
    });
    assert!(model.talk_setup.is_none());
    assert_eq!(model.talk_config.engine, TranscriptionEngine::Local);
    assert_eq!(
        model.talk_config.whisper_model_id.as_deref(),
        Some("base.en")
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|f| f.contains("talk ready"))
    );
}

/// MUTATION CHECK: mark the row Installed at ⏎ (skip the download).
/// Expected failure: no `InstallModel` request and no progress states.
#[test]
fn selecting_an_absent_model_downloads_with_progress() {
    let mut model = live_session();
    open_setup(&mut model);
    model.handle_talk(TalkEvent::SetupSnapshot {
        snapshot: snapshot(&[], Some("/opt/homebrew/bin/whisper-cli")),
    });
    model.handle(key(KeyCode::Enter)); // engine → local
    model.requests.clear();
    model.handle(key(KeyCode::Enter)); // tiny.en (absent) → download
    assert_eq!(
        talk_shell(&model.requests),
        vec![&TalkShellCommand::InstallModel {
            model_id: "tiny.en".to_owned()
        }]
    );
    let row_state = |model: &AppModel, index: usize| {
        model.talk_setup.as_ref().expect("open").whisper[index]
            .state
            .clone()
    };
    assert_eq!(
        row_state(&model, 0),
        WhisperRowState::Downloading { percent: None }
    );
    model.handle_talk(TalkEvent::DownloadProgress {
        model_id: "tiny.en".to_owned(),
        percent: Some(55),
    });
    assert_eq!(
        row_state(&model, 0),
        WhisperRowState::Downloading { percent: Some(55) }
    );
    model.handle_talk(TalkEvent::DownloadFinished {
        model_id: "tiny.en".to_owned(),
        error: None,
    });
    assert_eq!(row_state(&model, 0), WhisperRowState::Installed);
}

/// MUTATION CHECK: report a failed download as Installed. Expected
/// failure: the sha-mismatch text is lost and the row lies.
#[test]
fn a_failed_download_lands_on_the_row_honestly() {
    let mut model = live_session();
    open_setup(&mut model);
    model.handle_talk(TalkEvent::SetupSnapshot {
        snapshot: snapshot(&[], None),
    });
    model.handle(key(KeyCode::Enter));
    model.handle(key(KeyCode::Enter));
    model.handle_talk(TalkEvent::DownloadFinished {
        model_id: "tiny.en".to_owned(),
        error: Some("downloaded file failed checksum verification".to_owned()),
    });
    let card = model.talk_setup.as_ref().expect("open");
    assert_eq!(
        card.whisper[0].state,
        WhisperRowState::Failed("downloaded file failed checksum verification".to_owned())
    );
}

/// MUTATION CHECK: skip the runtime row's install path. Expected
/// failure: ⏎ on the missing whisper-cli row requests nothing.
#[test]
fn the_missing_runtime_row_drives_the_installer() {
    let mut model = live_session();
    open_setup(&mut model);
    model.handle_talk(TalkEvent::SetupSnapshot {
        snapshot: snapshot(&[], None),
    });
    model.handle(key(KeyCode::Enter)); // engine → local
    let card = model.talk_setup.as_ref().expect("open");
    assert!(matches!(card.runtime, RuntimeRowState::Missing(_)));
    // ↓↓↓ to the runtime row (index 3).
    for _ in 0..3 {
        model.handle(key(KeyCode::Down));
    }
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        talk_shell(&model.requests),
        vec![&TalkShellCommand::InstallRuntime]
    );
    assert_eq!(
        model.talk_setup.as_ref().expect("open").runtime,
        RuntimeRowState::Installing
    );
    model.handle_talk(TalkEvent::RuntimeInstalled {
        outcome: Ok(Some("/opt/homebrew/bin/whisper-cli".to_owned())),
        hint: None,
    });
    assert_eq!(
        model.talk_setup.as_ref().expect("open").runtime,
        RuntimeRowState::Found("/opt/homebrew/bin/whisper-cli".to_owned())
    );
}

/// The full Deepgram flow: paste → ⏎ probes (validate + models) → the
/// accepted key vaults through the daemon → models stage → select →
/// language → StoreConfig.
///
/// MUTATION CHECK: vault the key BEFORE validation (reorder). Expected
/// failure: the store request precedes `KeyAccepted` and an invalid key
/// would be vaulted.
#[test]
fn key_paste_validate_vault_models_language_flow() {
    let mut model = live_session();
    open_setup(&mut model);
    model.handle_talk(TalkEvent::SetupSnapshot {
        snapshot: snapshot(&[], None),
    });
    // Engine → deepgram.
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    let card = model.talk_setup.as_ref().expect("open");
    assert_eq!(card.stage, SetupStage::DeepgramKey);
    assert_eq!(card.key_stage, KeyStage::Entry);
    // Paste the key (keys are pasted more often than typed).
    model.handle(AppEvent::Paste(haider_tui::app::Pasted::new(
        "dg-secret-123".to_owned(),
    )));
    assert_eq!(model.talk_setup.as_ref().expect("open").masked_len(), 13);
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    // The probe carries the ONE live copy; the card's buffer emptied.
    let commands = talk_shell(&model.requests);
    assert!(matches!(
        commands.as_slice(),
        [TalkShellCommand::ProbeKey { .. }]
    ));
    assert!(
        !model
            .requests
            .iter()
            .any(|r| matches!(r, AppRequest::TranscriptionSecretStore { .. })),
        "nothing is vaulted before validation"
    );
    let card = model.talk_setup.as_ref().expect("open");
    assert_eq!(card.key_stage, KeyStage::Validating);
    assert_eq!(card.masked_len(), 0, "the card's copy was TAKEN");
    // The engine mock accepts and returns the model list.
    model.requests.clear();
    model.handle_talk(TalkEvent::KeyAccepted {
        secret: SecretWire::new("dg-secret-123"),
        models: models(),
    });
    let store = model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::TranscriptionSecretStore { secret, clear } => {
                Some((secret.expose_secret().to_owned(), *clear))
            }
            _ => None,
        })
        .expect("the accepted key vaults through the daemon");
    assert_eq!(store, ("dg-secret-123".to_owned(), false));
    assert_eq!(
        model.talk_setup.as_ref().expect("open").key_stage,
        KeyStage::Storing
    );
    // The daemon confirms → models stage.
    model.talk_secret_stored(true);
    let card = model.talk_setup.as_ref().expect("open");
    assert_eq!(card.stage, SetupStage::DeepgramModels);
    assert_eq!(card.models.len(), 2);
    assert!(card.key_present);
    // ↓ to nova-3-medical, ⏎ selects → language stage.
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    let card = model.talk_setup.as_ref().expect("open");
    assert_eq!(card.stage, SetupStage::Language);
    assert_eq!(
        card.config.deepgram_model.as_deref(),
        Some("nova-3-medical")
    );
    // The language field is prefilled `en`; extend it and save.
    model.handle(key(KeyCode::Char('-')));
    model.handle(key(KeyCode::Char('U')));
    model.handle(key(KeyCode::Char('S')));
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    let commands = talk_shell(&model.requests);
    let TalkShellCommand::StoreConfig { config } = commands[0] else {
        panic!("expected StoreConfig, got {commands:?}");
    };
    assert_eq!(config.engine, TranscriptionEngine::Deepgram);
    assert_eq!(config.deepgram_model.as_deref(), Some("nova-3-medical"));
    assert_eq!(config.language, "en-US");
    model.handle_talk(TalkEvent::ConfigStored {
        config: config.clone(),
        error: None,
    });
    assert!(model.talk_setup.is_none());
    assert_eq!(model.talk_config.engine, TranscriptionEngine::Deepgram);
}

/// MUTATION CHECK: keep `Validating` on rejection. Expected failure: a
/// 401 wedges the card with no retype path.
#[test]
fn a_rejected_key_returns_to_entry_with_the_honest_reason() {
    let mut model = live_session();
    open_setup(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    model.handle(AppEvent::Paste(haider_tui::app::Pasted::new(
        "bad-key".to_owned(),
    )));
    model.handle(key(KeyCode::Enter));
    model.handle_talk(TalkEvent::KeyRejected {
        error: SttError::Unauthorized("Deepgram rejected the API key (401)".to_owned()),
    });
    let card = model.talk_setup.as_ref().expect("open");
    assert_eq!(card.key_stage, KeyStage::Entry);
    assert!(card.error.as_deref().is_some_and(|e| e.contains("401")));
}

/// The reuse path: a vaulted key probes via the daemon read and NEVER
/// re-vaults.
///
/// MUTATION CHECK: run the store on the reuse path too. Expected
/// failure: a redundant `TranscriptionSecretStore` is issued.
#[test]
fn reusing_the_vaulted_key_skips_the_store() {
    let mut model = live_session();
    open_setup(&mut model);
    // The presence read answers: a key is vaulted.
    model.talk_secret_answer(Some(SecretWire::new("vaulted-key")));
    // Engine → deepgram: the key stage opens in Reuse.
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    let card = model.talk_setup.as_ref().expect("open");
    assert_eq!(card.key_stage, KeyStage::Reuse);
    // ⏎ reuse → a fresh vault read with the probe intent.
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::TranscriptionSecretRead]
    ));
    model.requests.clear();
    model.talk_secret_answer(Some(SecretWire::new("vaulted-key")));
    let commands = talk_shell(&model.requests);
    assert!(matches!(
        commands.as_slice(),
        [TalkShellCommand::ProbeKey { .. }]
    ));
    model.requests.clear();
    model.handle_talk(TalkEvent::KeyAccepted {
        secret: SecretWire::new("vaulted-key"),
        models: models(),
    });
    assert!(
        !model
            .requests
            .iter()
            .any(|r| matches!(r, AppRequest::TranscriptionSecretStore { .. })),
        "the vaulted key is not re-vaulted"
    );
    assert_eq!(
        model.talk_setup.as_ref().expect("open").stage,
        SetupStage::DeepgramModels
    );
}

/// `r` on the reuse row abandons the vaulted key for a retype.
#[test]
fn retype_leaves_the_reuse_path() {
    let mut model = live_session();
    open_setup(&mut model);
    model.talk_secret_answer(Some(SecretWire::new("vaulted-key")));
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    model.handle(key(KeyCode::Char('r')));
    let card = model.talk_setup.as_ref().expect("open");
    assert_eq!(card.key_stage, KeyStage::Entry);
    assert!(!card.key_reused);
}

/// MUTATION CHECK: let the deepgram row through without the feature.
/// Expected failure: no error and the key stage opens against a daemon
/// with nowhere to vault the key.
#[test]
fn deepgram_without_the_vault_feature_refuses_at_the_picker() {
    let mut model = live_session();
    model.daemon_features.clear();
    open_setup(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    let card = model.talk_setup.as_ref().expect("open");
    assert_eq!(
        card.stage,
        SetupStage::Engine,
        "the stage refuses to advance"
    );
    assert!(
        card.error
            .as_deref()
            .is_some_and(|e| e.contains("transcription_v1"))
    );
}

/// MUTATION CHECK: accept any language text. Expected failure: an
/// illegal language saves and the wire refuses it later, far from the
/// field.
#[test]
fn the_language_field_validates_before_saving() {
    let mut model = live_session();
    open_setup(&mut model);
    let card = model.talk_setup.as_mut().expect("open");
    card.stage = SetupStage::Language;
    card.language = String::new();
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    let card = model.talk_setup.as_ref().expect("open");
    assert!(
        card.error
            .as_deref()
            .is_some_and(|e| e.contains("language"))
    );
    assert!(talk_shell(&model.requests).is_empty());
    // Illegal characters never enter the field from the keyboard.
    model.handle(key(KeyCode::Char('e')));
    model.handle(key(KeyCode::Char('!')));
    model.handle(key(KeyCode::Char('n')));
    assert_eq!(model.talk_setup.as_ref().expect("open").language, "en");
}

/// MUTATION CHECK: leave the card open on Esc. Expected failure: the
/// modality never releases the band.
#[test]
fn esc_closes_the_card() {
    let mut model = live_session();
    open_setup(&mut model);
    model.handle(key(KeyCode::Esc));
    assert!(model.talk_setup.is_none());
}

/// The card's Debug is redacted by construction (the LoginCard law).
///
/// MUTATION CHECK: derive Debug on `TalkSetupCard`. Expected failure:
/// the key text prints.
#[test]
fn the_card_debug_never_prints_the_key() {
    let mut model = live_session();
    open_setup(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    model.handle(AppEvent::Paste(haider_tui::app::Pasted::new(
        "hyper-secret-deepgram-key".to_owned(),
    )));
    let debug = format!("{:?}", model.talk_setup.as_ref().expect("open"));
    assert!(!debug.contains("hyper-secret"), "redacted: {debug}");
    assert!(debug.contains("<redacted>"));
    // The whole-model Debug (panic teardown) is covered by the same law.
    let model_debug = format!("{model:?}");
    assert!(!model_debug.contains("hyper-secret"));
}

/// A corrupt profile section surfaces TYPED at `/talk`, and the setup
/// card carries it — never silently defaulted.
#[test]
fn a_corrupt_config_section_is_surfaced_not_defaulted() {
    let mut model = live_session();
    model.talk_config_error = Some("transcription config section is invalid".to_owned());
    submit(&mut model, "/talk");
    assert_eq!(model.talk.phase, haider_tui::talk::TalkPhase::Idle);
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|f| f.contains("config is broken")),
        "typed error surfaces: {:?}",
        model.flash
    );
}
