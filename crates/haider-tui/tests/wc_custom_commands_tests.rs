//! W-C M1 — custom slash commands: parse, namespacing, project-over-global
//! precedence, argument substitution, malformed skip-with-warning, palette
//! listing, and the model-override-reaches-the-turn law.
#![allow(clippy::expect_used)]

use haider_protocol::ids::SessionId;
use haider_tui::app::{AppModel, AppRequest, RuntimeMode, Screen};
use haider_tui::commands::{DynamicSlots, palette_items};
use haider_tui::custom_commands::{
    CommandSource, CustomCommand, ParseError, load, load_for, parse_command_file, substitute,
};
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// Pure module laws
// ---------------------------------------------------------------------------

#[test]
fn substitution_covers_all_three_forms_and_empty_positionals() {
    // $ARGUMENTS (all, space-joined), $N (positional), $ARGUMENTS[N] (alias).
    assert_eq!(
        substitute("all: $ARGUMENTS", &["a", "b", "c"]),
        "all: a b c"
    );
    assert_eq!(substitute("$1 then $2", &["x", "y"]), "x then y");
    assert_eq!(substitute("$ARGUMENTS[3]", &["x", "y", "z"]), "z");
    // Unfilled positionals expand to empty; a bare `$` stays literal.
    assert_eq!(
        substitute("[$2] [$ARGUMENTS[5]] $ ok", &["only"]),
        "[] [] $ ok"
    );
}

#[test]
fn parse_reads_cc_frontmatter_and_body() {
    // A verbatim Claude-Code command file drops in unchanged.
    let parsed = parse_command_file(
        "---\ndescription: Review a PR\nargument-hint: <pr-number>\nmodel: fable-5\nallowed-tools: Read, Bash(git*)\n---\nReview PR #$1 carefully.\n",
    )
    .expect("parse");
    assert_eq!(parsed.description.as_deref(), Some("Review a PR"));
    assert_eq!(parsed.argument_hint.as_deref(), Some("<pr-number>"));
    assert_eq!(parsed.model.as_deref(), Some("fable-5"));
    assert_eq!(parsed.allowed_tools, vec!["Read", "Bash(git*)"]);
    assert_eq!(parsed.body.trim(), "Review PR #$1 carefully.");
}

#[test]
fn parse_rejects_unterminated_frontmatter_and_empty_body() {
    assert_eq!(
        parse_command_file("---\ndescription: x\nno closing fence"),
        Err(ParseError::UnterminatedFrontmatter)
    );
    assert_eq!(
        parse_command_file("---\ndescription: x\n---\n   \n"),
        Err(ParseError::EmptyBody)
    );
}

fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, contents).expect("write");
}

#[test]
fn subdirectory_becomes_a_colon_namespace() {
    let temp = tempfile::tempdir().expect("temp");
    let dir = temp.path();
    write(dir, "deploy.md", "Deploy now");
    write(dir, "git/commit.md", "Commit $ARGUMENTS");
    let result = load(Some(dir), None);
    let names: Vec<&str> = result
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    assert!(names.contains(&"deploy"), "flat name: {names:?}");
    assert!(names.contains(&"git:commit"), "namespaced: {names:?}");
    assert!(
        result.warnings.is_empty(),
        "warnings: {:?}",
        result.warnings
    );
}

#[test]
fn project_wins_over_global_on_name_collision() {
    let global = tempfile::tempdir().expect("temp");
    let project = tempfile::tempdir().expect("temp");
    write(global.path(), "greet.md", "GLOBAL greeting");
    write(project.path(), "greet.md", "PROJECT greeting");
    let result = load(Some(project.path()), Some(global.path()));
    let greet = result
        .commands
        .iter()
        .find(|command| command.name == "greet")
        .expect("greet present");
    assert_eq!(greet.body.trim(), "PROJECT greeting");
    assert_eq!(greet.source, CommandSource::Project);
    // Exactly one `greet` survives — the project shadowed the global.
    assert_eq!(
        result
            .commands
            .iter()
            .filter(|command| command.name == "greet")
            .count(),
        1
    );
}

#[test]
fn malformed_file_is_skipped_with_a_surfaced_warning() {
    let temp = tempfile::tempdir().expect("temp");
    let dir = temp.path();
    write(dir, "good.md", "A fine command");
    // Unterminated frontmatter — malformed.
    write(dir, "bad.md", "---\ndescription: broken\nno fence here");
    let result = load(Some(dir), None);
    let names: Vec<&str> = result
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    assert_eq!(names, vec!["good"], "the good command still loads");
    assert_eq!(
        result.warnings.len(),
        1,
        "one warning: {:?}",
        result.warnings
    );
    assert!(
        result.warnings[0].contains("bad.md"),
        "warning names the file: {}",
        result.warnings[0]
    );
}

#[test]
fn load_for_walks_up_to_the_project_root() {
    let temp = tempfile::tempdir().expect("temp");
    let root = temp.path();
    write(
        &root.join(".haider").join("commands"),
        "ship.md",
        "Ship it $ARGUMENTS",
    );
    // A deep working directory still finds the ancestor `.haider/commands`.
    let deep = root.join("crates").join("worker").join("src");
    fs::create_dir_all(&deep).expect("mkdir deep");
    let result = load_for(Some(&deep), None);
    assert!(
        result.commands.iter().any(|command| command.name == "ship"),
        "found via walk-up: {:?}",
        result
            .commands
            .iter()
            .map(|command| command.name.clone())
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// App integration laws
// ---------------------------------------------------------------------------

fn custom(name: &str, body: &str) -> CustomCommand {
    CustomCommand {
        name: name.to_owned(),
        description: Some(format!("{name} description")),
        argument_hint: None,
        model: None,
        allowed_tools: Vec::new(),
        body: body.to_owned(),
        source: CommandSource::Project,
    }
}

fn live_session_model() -> AppModel {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Session;
    model.active_session = Some(SessionId::new("s-1"));
    model
}

fn provider_summary(provider: &str, models: Vec<String>) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: None,
        model_details: Vec::new(),
        models,
        auth_methods: Vec::new(),
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: None,
        enabled: true,
    }
}

fn run_line(model: &mut AppModel, line: &str) {
    use ratatui::crossterm::event::KeyCode;
    for c in line.chars() {
        model.handle(haider_tui::app::AppEvent::Key(
            ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ),
        ));
    }
    // Esc dismisses the palette so ⏎ runs the typed line, not a palette row.
    model.handle(haider_tui::app::AppEvent::Key(
        ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Esc,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ),
    ));
    model.handle(haider_tui::app::AppEvent::Key(
        ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ),
    ));
}

#[test]
fn palette_lists_custom_commands_visually_distinct() {
    let slots = DynamicSlots {
        custom_commands: vec![("deploy".to_owned(), "Deploy the app".to_owned())],
        ..DynamicSlots::default()
    };
    let items = palette_items("dep", true, &slots);
    let custom = items
        .iter()
        .find(|item| item.label() == "/deploy")
        .expect("custom command listed");
    assert!(custom.is_custom(), "marked as custom, not a built-in");
    assert_eq!(custom.desc(), "Deploy the app");
    // A built-in matching the same prefix is NOT custom (never replaced).
    assert!(
        palette_items("mo", true, &slots)
            .iter()
            .filter(|item| !item.is_custom())
            .any(|item| item.label() == "/model"),
        "built-ins still present"
    );
}

#[test]
fn custom_command_expands_and_submits_a_user_turn() {
    let mut model = live_session_model();
    model.set_custom_commands(
        vec![custom("greet", "Hello, $1! Ready for $ARGUMENTS?")],
        Vec::new(),
    );
    run_line(&mut model, "/greet Sam and friends");
    let submitted = model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::SubmitText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("a user turn was submitted");
    // NO inline execution — the body is the substituted PROMPT.
    assert_eq!(submitted, "Hello, Sam! Ready for Sam and friends?");
}

#[test]
fn custom_command_model_override_reaches_the_turn() {
    let mut model = live_session_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    model.providers.apply_snapshot(
        vec![provider_summary("openai", vec!["gpt-5.6".to_owned()])],
        1,
    );
    let mut command = custom("audit", "Audit $ARGUMENTS");
    command.model = Some("gpt-5.6".to_owned());
    model.set_custom_commands(vec![command], Vec::new());
    run_line(&mut model, "/audit the crate");
    // The G3 select_model request rides AHEAD of the submit.
    let select_pos = model.requests.iter().position(|request| {
        matches!(request, AppRequest::SelectModel { model, provider, .. } if model == "gpt-5.6" && provider == "openai")
    });
    let submit_pos = model
        .requests
        .iter()
        .position(|request| matches!(request, AppRequest::SubmitText { .. }));
    assert!(
        select_pos.is_some(),
        "model override reached the turn: {:?}",
        model.requests
    );
    assert!(
        select_pos < submit_pos,
        "select_model precedes the submit: {:?}",
        model.requests
    );
}

#[test]
fn custom_command_unknown_model_is_ignored_with_a_note() {
    let mut model = live_session_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    model.providers.apply_snapshot(
        vec![provider_summary("openai", vec!["gpt-5.6".to_owned()])],
        1,
    );
    let mut command = custom("audit", "Audit $ARGUMENTS");
    command.model = Some("no-such-model".to_owned());
    model.set_custom_commands(vec![command], Vec::new());
    run_line(&mut model, "/audit the crate");
    // The turn still goes out (the override is ignored, not fatal)...
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SubmitText { .. })),
        "the turn still submits"
    );
    // ...and NO model selection was issued.
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SelectModel { .. })),
        "no select_model for an unknown model"
    );
    let note = model.flash.clone().unwrap_or_default();
    assert!(
        note.contains("unknown"),
        "ignored-with-note flash: {note:?}"
    );
}

#[test]
fn set_custom_commands_surfaces_a_warning_flash() {
    let mut model = AppModel::new();
    model.set_custom_commands(Vec::new(), vec!["bad.md: empty command body".to_owned()]);
    let flash = model.flash.clone().expect("warning flash");
    assert!(flash.contains("skipped"), "flash: {flash}");
    assert_eq!(model.custom_command_warnings.len(), 1);
}
