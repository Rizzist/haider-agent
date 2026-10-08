//! A deferred spend card waits below local pickers. Its visibility is frozen
//! at event entry, so picker gestures cannot navigate or answer it.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{CredentialAlias, MenuId, SessionId};
use haider_protocol::menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
use haider_rpc::SurfaceInjectOp;
use haider_tui::app::{AppEvent, AppModel, ChipModel, ChipQuestion, Hit, RuntimeMode, Screen};
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::script::{ChipDisplayState, ChipSeed};
use ratatui::crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};

mod common;
use common::{key, launcher_model, run_slash, submit};

#[derive(Clone, Copy, Debug)]
enum Picker {
    Theme,
    Effort,
    Model,
}

impl Picker {
    fn slash(self) -> &'static str {
        match self {
            Self::Theme => "/theme",
            Self::Effort => "/effort",
            Self::Model => "/model",
        }
    }

    fn is_open(self, model: &AppModel) -> bool {
        match self {
            Self::Theme => model.theme_picker.is_some(),
            Self::Effort => model.effort_picker.is_some(),
            Self::Model => model.model_picker.is_some(),
        }
    }

    fn painted_marker(self) -> &'static str {
        match self {
            Self::Theme => "theme — how haider dresses this terminal",
            Self::Effort => "effort — reasoning depth for this pair",
            Self::Model => "OAuth subscriptions + one API choice per model",
        }
    }

    fn stale_hit(self, model: &AppModel) -> Hit {
        match self {
            Self::Theme => Hit::ThemeOption(0),
            Self::Effort => Hit::EffortOption(0),
            Self::Model => {
                let row = model
                    .model_picker_rows()
                    .into_iter()
                    .find(|row| row.auth == "oauth" && row.selectable)
                    .expect("selectable OAuth model row");
                Hit::ModelPickerRow {
                    provider: row.provider,
                    model: row.model,
                    api_group: false,
                }
            }
        }
    }
}

fn reset_card() -> Menu {
    Menu {
        id: MenuId::new("deferred-picker-reset"),
        kind: MenuKind::DeferredApproval {
            approval_id: "picker-reset".into(),
            effect: EffectClass::UsageResetApply,
            account: Some(CredentialAlias::new("synthetic-account")),
            binding: None,
        },
        title: "Apply a usage reset credit?".into(),
        body: vec!["Account: synthetic-account".into()],
        options: vec![
            MenuOption {
                key: "deny".into(),
                label: "Deny".into(),
                detail: None,
                decision: Some(DecisionKind::RejectOnce),
            },
            MenuOption {
                key: "approve".into(),
                label: "Apply one reset".into(),
                detail: None,
                decision: Some(DecisionKind::AllowOnce),
            },
        ],
        blocking: false,
        scope: MenuScope::Session,
        origin: "deferred_approval_v1".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn painted(model: &AppModel) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| {
            haider_tui::render::render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn seeded_session() -> AppModel {
    let mut model = launcher_model();
    submit(&mut model, "work on the repo");
    model.composer = haider_tui::composer::Composer::default();
    model.outbox.clear();
    model.requests.clear();
    model.turn_active = false;
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        "provider_models_v1",
        "session_model_select_v1",
        "session_effort_select_v1",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    model.daemon_version = Some("synthetic".into());
    let mut summaries = seed_provider_summaries();
    let anthropic = summaries
        .iter_mut()
        .find(|row| row.provider == "anthropic")
        .expect("anthropic summary");
    for detail in &mut anthropic.model_details {
        detail.supported_efforts = vec!["low".into(), "medium".into(), "high".into()];
        detail.default_effort = Some("medium".into());
    }
    model.providers.apply_snapshot(summaries, 1);
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.identity.provider = "anthropic".into();
    model.identity.model_short = "claude-opus-5".into();
    model.identity_pinned = true;
    model
}

fn chip_with_menu(model: &mut AppModel, menu: Menu) {
    // The parent session may have its own ordinary question while the chip
    // view is active. Only the viewed chip's menu participates in ownership.
    let mut parent_question = reset_card();
    parent_question.id = MenuId::new("parent-question-behind-chip");
    parent_question.kind = MenuKind::Question;
    parent_question.blocking = true;
    model
        .projection
        .apply(&EventPayload::MenuOpened(parent_question));
    let mut chip = ChipModel::from_seed(ChipSeed {
        agent: "agent-reset".into(),
        parent: None,
        ros: None,
        callsign: "Husayn".into(),
        hon: "(r)",
        full: "Husayn ibn Ali".into(),
        name: "reset probe".into(),
        model: "fable-5".into(),
        device: "local".into(),
        state: ChipDisplayState::InputRequired,
        tokens: 10,
        prefill: Vec::new(),
    });
    chip.question = Some(ChipQuestion {
        recovery: false,
        text: "Apply a usage reset credit?".into(),
        options: vec!["Deny".into(), "Apply one reset".into()],
        resolved: false,
    });
    chip.transcript.apply(&EventPayload::MenuOpened(menu));
    model.chips.push(chip);
    model.screen = Screen::Subagent;
    model.view_path = vec!["agent-reset".into()];
    assert!(
        model
            .viewed_chip()
            .and_then(ChipModel::question_menu)
            .is_some()
    );
}

fn chip_with_card(model: &mut AppModel) {
    chip_with_menu(model, reset_card());
}

fn open_picker_then_card(picker: Picker, chip: bool) -> AppModel {
    let mut model = seeded_session();
    run_slash(&mut model, picker.slash());
    assert!(picker.is_open(&model), "{} precondition", picker.slash());
    if chip {
        chip_with_card(&mut model);
    } else {
        model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            reset_card(),
        ))));
    }
    model.outbox.clear();
    let frame = painted(&model);
    assert!(
        frame.contains(picker.painted_marker()),
        "{} must stay painted over the card:\n{frame}",
        picker.slash()
    );
    assert!(
        !frame.contains("Apply one reset"),
        "spend card must wait below {}:\n{frame}",
        picker.slash()
    );
    model
}

fn assert_picker_owns_each_gesture(picker: Picker, chip: bool) {
    let mut model = open_picker_then_card(picker, chip);
    model.handle(key(KeyCode::Down));
    assert!(picker.is_open(&model), "Down stays with the picker");
    assert!(model.outbox.is_empty());
    model.handle(key(KeyCode::Enter));
    assert!(model.outbox.is_empty(), "picker Down+Enter answered card");

    let mut model = open_picker_then_card(picker, chip);
    model.handle(key(KeyCode::Enter));
    assert!(model.outbox.is_empty(), "picker Enter answered card");

    let mut model = open_picker_then_card(picker, chip);
    model.handle(key(KeyCode::Esc));
    assert!(model.outbox.is_empty(), "picker Esc answered card");
    assert!(!picker.is_open(&model), "Esc closes picker first");
    let frame = painted(&model);
    assert!(
        frame.contains("Deny"),
        "pending card returns at Deny:\n{frame}"
    );

    let mut model = open_picker_then_card(picker, chip);
    let session = SessionId::new("synthetic-picker-injection");
    model.active_session = Some(session.clone());
    LiveDriver::new("picker-test").apply(
        &mut model,
        LiveReply::InputInjected {
            session,
            op: SurfaceInjectOp::Submit,
        },
    );
    assert!(
        model.outbox.is_empty(),
        "injected Submit answered hidden card"
    );
    assert!(
        picker.is_open(&model),
        "injected Submit must leave picker open"
    );
}

#[test]
fn session_theme_picker_owns_spend_card_gestures() {
    assert_picker_owns_each_gesture(Picker::Theme, false);
}

#[test]
fn session_effort_picker_owns_spend_card_gestures() {
    assert_picker_owns_each_gesture(Picker::Effort, false);
}

#[test]
fn session_model_picker_owns_spend_card_gestures() {
    assert_picker_owns_each_gesture(Picker::Model, false);
}

#[test]
fn chip_theme_picker_owns_spend_card_gestures() {
    assert_picker_owns_each_gesture(Picker::Theme, true);
}

#[test]
fn chip_effort_picker_owns_spend_card_gestures() {
    assert_picker_owns_each_gesture(Picker::Effort, true);
}

#[test]
fn chip_model_picker_owns_spend_card_gestures() {
    assert_picker_owns_each_gesture(Picker::Model, true);
}

#[test]
fn blocking_question_still_outranks_a_local_picker() {
    for picker in [Picker::Theme, Picker::Effort, Picker::Model] {
        let mut model = seeded_session();
        run_slash(&mut model, picker.slash());
        let mut question = reset_card();
        question.kind = MenuKind::Question;
        question.blocking = true;
        question.options[1].key = "second".into();
        model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            question,
        ))));
        let frame = painted(&model);
        assert!(frame.contains("Deny"), "question stays painted:\n{frame}");
        assert!(
            !frame.contains(picker.painted_marker()),
            "{picker:?} waits below question"
        );
        let stale_hit = picker.stale_hit(&model);
        model.handle_hit(stale_hit);
        assert!(picker.is_open(&model), "{picker:?} stale hit must drop");
        assert!(
            model.outbox.is_empty(),
            "{picker:?} stale hit must not answer"
        );
        model.handle(key(KeyCode::Down));
        assert!(!picker.is_open(&model));
        model.handle(key(KeyCode::Enter));
        assert_eq!(
            model.outbox[0].answer.option_key.as_deref(),
            Some("second"),
            "{picker:?} must forward Down to the painted question"
        );
    }
}

#[test]
fn blocking_chip_question_still_outranks_a_local_picker() {
    for picker in [Picker::Theme, Picker::Effort, Picker::Model] {
        let mut model = seeded_session();
        run_slash(&mut model, picker.slash());
        let mut question = reset_card();
        question.kind = MenuKind::Question;
        question.blocking = true;
        question.options[1].key = "second".into();
        chip_with_menu(&mut model, question);
        model.outbox.clear();
        let frame = painted(&model);
        assert!(
            frame.contains("Deny"),
            "chip question stays painted:\n{frame}"
        );
        assert!(
            !frame.contains(picker.painted_marker()),
            "{picker:?} waits below chip question"
        );
        let stale_hit = picker.stale_hit(&model);
        model.handle_hit(stale_hit);
        assert!(
            picker.is_open(&model),
            "{picker:?} stale chip hit must drop"
        );
        assert!(
            model.outbox.is_empty(),
            "{picker:?} stale hit must not answer"
        );
        model.handle(key(KeyCode::Down));
        assert!(!picker.is_open(&model));
        model.handle(key(KeyCode::Enter));
        assert_eq!(
            model.outbox[0].answer.option_key.as_deref(),
            Some("second"),
            "{picker:?} must forward Down to the painted chip question"
        );
    }
}
