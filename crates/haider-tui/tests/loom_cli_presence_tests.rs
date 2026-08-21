//! W-flow — declared CLIs are a capability grant, not a promise the program
//! is INSTALLED. `loom.list` now carries a device presence map, the type
//! detail draws it, and ⌃I seeds a provisioning turn for exactly the missing
//! programs.
//!
//! The load-bearing distinction throughout: **unknown is not missing.** A
//! name the daemon never probed (an older daemon sends no map) must render
//! as nothing at all and must never be offered for install — otherwise a
//! silent wire gap becomes a confident wrong claim about the operator's
//! machine.

#![allow(clippy::expect_used)]

use haider_protocol::loom::LoomAgentType;
use haider_tui::app::{AppModel, LoomPane, RuntimeMode, Screen};
use haider_tui::render::{missing_clis, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{ctrl, launcher_model};

fn typed(id: &str, clis: &[&str]) -> LoomAgentType {
    LoomAgentType {
        id: id.into(),
        name: id.into(),
        job: "Pull a source and transcribe it.".into(),
        in_type: "SourceURL".into(),
        out_type: "Transcript".into(),
        clis: clis.iter().map(|cli| (*cli).to_owned()).collect(),
        apis: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: "#c2701c".into(),
        glyph: "▲".into(),
        rev: 1,
    }
}

fn model_with(record: LoomAgentType, present: &[(&str, bool)]) -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_LOOM_CLI_PRESENCE_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.loom_loaded = true;
    model.loom_types = vec![record];
    model.loom_cli_present = present
        .iter()
        .map(|(cli, ok)| ((*cli).to_owned(), *ok))
        .collect();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model
}

fn draw(model: &AppModel) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(110, 46)).expect("terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("render");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|row| {
            (0..buffer.area.width)
                .map(|col| buffer[(col, row)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

/// MUTATION CHECK (executed): make `missing_clis` treat an ABSENT map entry
/// as missing (`!= Some(&true)` instead of `== Some(&false)`). Expected
/// RUNTIME failure: the unprobed assertion below — `ffmpeg` would be
/// reported missing on a device nobody ever asked about it.
#[test]
fn unprobed_is_never_reported_missing() {
    let record = typed("scout", &["yt-dlp", "ffmpeg", "jq"]);
    // yt-dlp probed absent, jq probed present, ffmpeg NOT PROBED at all.
    let model = model_with(record.clone(), &[("yt-dlp", false), ("jq", true)]);

    assert_eq!(
        missing_clis(&model, &model.loom_types[0]),
        vec!["yt-dlp".to_owned()],
        "only a PROBED-absent name is missing"
    );

    // An older daemon sends no map: nothing is missing, because nothing was
    // checked — not everything, because nothing was found.
    let blind = model_with(record, &[]);
    assert!(
        missing_clis(&blind, &blind.loom_types[0]).is_empty(),
        "an unprobed registry reports NO missing programs"
    );
}

/// MUTATION CHECK (executed): drop the `None => {}` arm's distinction and
/// label unprobed names as missing. Expected RUNTIME failure: the
/// unprobed-name assertion — `ffmpeg` would carry the ✗ chip.
#[test]
fn the_detail_marks_present_missing_and_stays_silent_on_unprobed() {
    let mut model = model_with(
        typed("scout", &["yt-dlp", "ffmpeg", "jq"]),
        &[("yt-dlp", false), ("jq", true)],
    );
    model.loom_selection = 1; // the synthetic `∅ none` row leads
    model.loom_detail = true;
    let rows = draw(&model);
    let all = rows.join("\n");

    let line_for = |needle: &str| -> String {
        rows.iter()
            .find(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row for {needle}:\n{all}"))
            .clone()
    };
    assert!(
        line_for("yt-dlp").contains("✗ not on this device"),
        "a probed-absent CLI is marked:\n{all}"
    );
    assert!(
        line_for("jq").contains("✓ installed"),
        "a probed-present CLI is marked:\n{all}"
    );
    let ffmpeg = line_for("ffmpeg");
    assert!(
        !ffmpeg.contains('✗') && !ffmpeg.contains('✓'),
        "an UNPROBED CLI carries no verdict either way: {ffmpeg}"
    );
    assert!(
        all.contains("⌃I") && all.contains("install the 1 missing program"),
        "the install affordance counts only probed-absent names:\n{all}"
    );
}

/// ⌃I seeds a turn naming EXACTLY the missing programs — the install then
/// runs behind the ordinary process_exec permission card, which is the
/// confirmation the owner asked for.
///
/// MUTATION CHECK (executed): seed every declared CLI instead of the missing
/// ones. Expected RUNTIME failure: the `jq`/`ffmpeg` exclusion assertions.
#[test]
fn ctrl_i_seeds_a_turn_for_exactly_the_missing_programs() {
    let mut model = model_with(
        typed("scout", &["yt-dlp", "ffmpeg", "jq"]),
        &[("yt-dlp", false), ("jq", true)],
    );
    model.upsert_live_session(&haider_protocol::ids::SessionId::new("s-cli"));
    model.open_session(&haider_protocol::ids::SessionId::new("s-cli"));
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.loom_selection = 1;
    model.requests.clear();

    model.handle(ctrl(KeyCode::Char('i')));
    let seeded = model.composer.text().to_owned();
    assert!(
        seeded.contains("yt-dlp"),
        "the missing program is named: {seeded}"
    );
    assert!(
        !seeded.contains("jq"),
        "an INSTALLED program is never offered for install: {seeded}"
    );
    assert!(
        !seeded.contains("ffmpeg"),
        "an UNPROBED program is never offered for install: {seeded}"
    );
    assert!(
        model.requests.is_empty(),
        "seeding reaches no wire — the turn and its permission card do: {:?}",
        model.requests
    );
    assert_eq!(model.screen, Screen::Loom, "authoring stays in the tab");

    // Nothing missing → an honest flash, and the composer is left alone.
    let mut clean = model_with(typed("scout", &["jq"]), &[("jq", true)]);
    clean.upsert_live_session(&haider_protocol::ids::SessionId::new("s-cli"));
    clean.open_session(&haider_protocol::ids::SessionId::new("s-cli"));
    clean.screen = Screen::Loom;
    clean.loom_pane = LoomPane::Types;
    clean.loom_selection = 1;
    clean.handle(ctrl(KeyCode::Char('i')));
    assert!(
        clean.composer.text().is_empty(),
        "nothing missing seeds nothing"
    );
    assert_eq!(
        clean.flash.as_deref(),
        Some("· @scout — nothing missing to install")
    );
}

/// The probe resolves like an exec would, without running a shell.
///
/// MUTATION CHECK (executed): drop the `is_file()` guard in
/// `is_executable_file`. Expected RUNTIME failure: the directory assertion —
/// a `PATH` entry that is a directory would count as the program.
#[test]
fn the_path_probe_matches_what_an_exec_would_find() {
    // `sh` is on PATH on every unix this ships to; a name with no plausible
    // installation is not.
    assert!(
        haider_platform::program_on_path("sh"),
        "a real program on PATH is found"
    );
    assert!(
        !haider_platform::program_on_path("haider-no-such-program-xyzzy"),
        "an absent program is absent"
    );
    assert!(
        !haider_platform::program_on_path(""),
        "the empty name is never a program"
    );
    // A directory is never a program, even at an absolute path.
    assert!(
        !haider_platform::program_on_path("/usr"),
        "a directory is not an executable"
    );
    // A path-shaped name is checked WHERE IT POINTS, not searched on PATH.
    assert!(
        !haider_platform::program_on_path("./sh"),
        "a relative path is not a PATH search"
    );
}
