#![allow(clippy::expect_used)]
//! HAIDER952PROVIDERUI — character-indexed custom-provider card editing.

use haider_tui::app::{AppModel, CustomField, Hit, RuntimeMode, Screen};
use haider_tui::render::render;
use haider_tui::runtime::dispatch_input;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

mod common;
use common::{key, launcher_model};

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 7);
    model.screen = Screen::Providers;
    model
}

fn provider_summary(provider: &str) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:9999/v1".to_owned()),
        response_open_timeout_ms: None,
        model_details: Vec::new(),
        models: vec!["model-a".to_owned()],
        inventory_fetched_at_ms: None,
        auth_methods: Vec::new(),
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("model-a".to_owned()),
        enabled: true,
    }
}

fn draw(model: &AppModel) -> (Vec<String>, Vec<(Rect, Hit)>) {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let rows = (0..buffer.area.height)
        .map(|y| {
            let mut row = String::new();
            for x in 0..buffer.area.width {
                row.push_str(buffer[(x, y)].symbol());
            }
            row
        })
        .collect();
    (rows, hits)
}

fn field_rect(hits: &[(Rect, Hit)], field: CustomField) -> Rect {
    hits.iter()
        .find_map(|(rect, hit)| match hit {
            Hit::CustomProviderField {
                field: rendered, ..
            } if *rendered == field => Some(*rect),
            _ => None,
        })
        .unwrap_or_else(|| panic!("editable {field:?} field hit"))
}

fn mouse_down(column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// MUTATION CHECK (click placement): make `focus_at` always set the cursor
/// to the field end, or make insertion append with `String::push`. Expected
/// runtime failure: the cursor/value assertions report 6 / `abcdefx` instead
/// of 3 / `abcxdef`. A click beyond the value must still clamp to its end.
#[test]
fn click_places_caret_at_character_and_past_end_clamps() {
    let mut model = live_model();
    model.handle(key(KeyCode::Char('o')));
    {
        let card = model.custom_add.as_mut().expect("preset card");
        card.name = "abcdef".to_owned();
    }

    let (_, hits) = draw(&model);
    let name = field_rect(&hits, CustomField::Name);
    dispatch_input(&mut model, &hits, mouse_down(name.x + 3, name.y));
    let card = model.custom_add.as_ref().expect("card remains open");
    assert_eq!(card.focus, CustomField::Name);
    assert_eq!(card.cursor, 3);

    let (rows, _) = draw(&model);
    assert!(
        rows.iter().any(|row| row.contains("abc▏def")),
        "the rendered caret follows the click:\n{}",
        rows.join("\n")
    );

    model.handle(key(KeyCode::Char('x')));
    let card = model.custom_add.as_ref().expect("card");
    assert_eq!(card.name, "abcxdef", "typing inserts at the caret");
    assert_eq!(card.cursor, 4);

    let (_, hits) = draw(&model);
    let name = field_rect(&hits, CustomField::Name);
    dispatch_input(&mut model, &hits, mouse_down(name.x + 60, name.y));
    let card = model.custom_add.as_ref().expect("card");
    assert_eq!(card.cursor, card.name.chars().count());
}

/// MUTATION CHECK (character, not byte): convert the character cursor
/// directly to a byte offset. Expected runtime failure: inserting at offset
/// 2 in `aébc` splits `é` (panic/not-a-boundary) instead of producing
/// `aéxbc`.
#[test]
fn multibyte_field_click_and_edit_never_split_utf8() {
    let mut model = live_model();
    model.handle(key(KeyCode::Char('o')));
    model.custom_add.as_mut().expect("preset card").origin = "aébc".to_owned();

    let (_, hits) = draw(&model);
    let origin = field_rect(&hits, CustomField::Origin);
    dispatch_input(&mut model, &hits, mouse_down(origin.x + 2, origin.y));
    let card = model.custom_add.as_ref().expect("card");
    assert_eq!(card.focus, CustomField::Origin);
    assert_eq!(card.cursor, 2);

    let (rows, _) = draw(&model);
    assert!(rows.iter().any(|row| row.contains("aé▏bc")));
    model.handle(key(KeyCode::Char('x')));
    assert_eq!(model.custom_add.as_ref().expect("card").origin, "aéxbc");
}

/// MUTATION CHECK (create-prefill editability): reject Name/Origin in
/// `can_edit_field` even when `edit == false`. Expected runtime failure: the
/// prefilled Ollama name/origin remain unchanged.
#[test]
fn create_preset_name_and_origin_prefills_are_editable() {
    let mut model = live_model();
    model.handle(key(KeyCode::Char('o')));

    // Model -> Name, then insert at the start of the prefilled name.
    model.handle(key(KeyCode::Tab));
    assert_eq!(
        model.custom_add.as_ref().expect("card").focus,
        CustomField::Name
    );
    model.handle(key(KeyCode::Home));
    model.handle(key(KeyCode::Char('x')));
    assert_eq!(model.custom_add.as_ref().expect("card").name, "xollama");

    // Name -> Origin, then edit the prefilled URL in place.
    model.handle(key(KeyCode::Tab));
    assert_eq!(
        model.custom_add.as_ref().expect("card").focus,
        CustomField::Origin
    );
    model.handle(key(KeyCode::End));
    model.handle(key(KeyCode::Backspace));
    assert_eq!(
        model.custom_add.as_ref().expect("card").origin,
        "http://127.0.0.1:11434/v"
    );
}

/// MUTATION CHECK (no rename): allow edit-mode Name in `can_edit_field`.
/// Expected runtime failure: the forced-focus character input changes
/// `stable-provider` to `stable-providerx`. Origin remains editable.
#[test]
fn existing_provider_origin_is_editable_but_name_is_not() {
    let mut model = live_model();
    model
        .providers
        .apply_snapshot(vec![provider_summary("stable-provider")], 7);
    model.handle(key(KeyCode::Char('e')));

    // The endpoint is a normal edit field.
    model.handle(key(KeyCode::Tab));
    model.handle(key(KeyCode::End));
    model.handle(key(KeyCode::Char('x')));
    assert!(
        model
            .custom_add
            .as_ref()
            .expect("card")
            .origin
            .ends_with('x')
    );

    // Defense in depth at the mutation seam: even a forced/stale Name
    // focus cannot rename an existing provider.
    {
        let card = model.custom_add.as_mut().expect("card");
        card.focus = CustomField::Name;
        card.cursor = card.name.chars().count();
    }
    model.handle(key(KeyCode::Char('x')));
    assert_eq!(
        model.custom_add.as_ref().expect("card").name,
        "stable-provider"
    );

    let (_, hits) = draw(&model);
    assert!(
        !hits.iter().any(|(_, hit)| matches!(
            hit,
            Hit::CustomProviderField {
                field: CustomField::Name,
                ..
            }
        )),
        "the locked identity line must not advertise an editable hit"
    );
}
