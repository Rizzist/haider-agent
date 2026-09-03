//! W5f-0 — the owner's three-screenshot bug report:
//!
//! 1. The OAuth card said "your browser opened auth.openai.com" and no
//!    browser ever opened: `run_live`'s shell executor swallowed
//!    `AppRequest::OpenUrl` in a `_ => {}` catch-all. The shell channel is
//!    now the CLOSED [`ShellRequest`] vocabulary (no catch-all can exist),
//!    and the browser hop itself is opener-injected so both outcomes pin.
//! 2. The launcher listed an ERRORED session as `running…` with the gold
//!    pulse: `SessionState::busy()` counted every non-"IDLE" badge as
//!    busy, and `✗ ERRORED` is terminal, so a dead turn pulsed forever.
#![allow(clippy::expect_used)]

use haider_protocol::ids::SessionId;
use haider_protocol::state::RunState;
use haider_protocol::{DeliveryMode, EventPayload};
use haider_tui::app::{AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::browser::open_url_command_with_env;
use haider_tui::identity::UiGeneration;
use haider_tui::live::LiveDriver;
use haider_tui::render::render;
use haider_tui::runtime::{ShellRequest, live_pass, open_url_effects};
use haider_tui::session::SessionState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::{launcher_model, run_slash};

fn errored_session() -> SessionState {
    let mut entry = SessionState::neutral(SessionId::new("s-errored"), UiGeneration::SCRATCH);
    entry.name = Some("session".to_owned());
    entry.absorb_envelope(&EventPayload::UserMessage {
        text: "hi".to_owned(),
        attachments: Vec::new(),
        mode: DeliveryMode::Steer,
    });
    assert!(
        entry.busy(),
        "a live turn IS busy — the fix must not eat it"
    );
    entry.absorb_envelope(&EventPayload::RunState(RunState::Errored));
    entry
}

fn draw_rows(model: &AppModel) -> Vec<String> {
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            let _: Vec<(ratatui::layout::Rect, Hit)> = render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

/// MUTATION CHECK (W5f-0): in `live_pass`'s request drain, remove the
/// `AppRequest::OpenUrl` arm so it falls through to
/// `driver.handle_request` (which discards it as "runtime-owned").
/// Expected runtime failure: `pass.shell` comes back empty — the browser
/// hop vanishes, which is EXACTLY the shipped bug, one seam earlier.
/// Verified by revert on 2026-07-30.
#[test]
fn the_oauth_authorize_hop_rides_to_the_shell_as_open_url() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    let mut driver = LiveDriver::new("test");
    model.requests.push(AppRequest::OpenUrl {
        url: "https://auth.openai.com/oauth/authorize?code_challenge=x".to_owned(),
    });
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    assert_eq!(
        pass.shell,
        vec![ShellRequest::OpenUrl(
            "https://auth.openai.com/oauth/authorize?code_challenge=x".to_owned()
        )],
        "the authorize URL must come back for the shell to open"
    );
}

/// MUTATION CHECK (W5f-0): make `open_url_effects` ignore the opener's
/// error (early-return unconditionally). Expected runtime failure: the
/// failure arm below finds no flash and no clipboard fallback — the user
/// is stranded behind copy that claims a browser opened.
/// Verified by revert on 2026-07-30.
#[test]
fn a_failed_browser_spawn_says_so_and_leaves_the_link_reachable() {
    let mut model = launcher_model();
    open_url_effects(&mut model, "https://auth.openai.com/x", &|_| {
        Err(std::io::Error::other("no browser"))
    });
    let flash = model.flash.as_deref().unwrap_or_default();
    assert!(
        flash.contains("couldn't open a browser"),
        "the failure must be narrated, not swallowed: {flash:?}"
    );
    assert!(
        flash.contains("clipboard"),
        "and must say where the link went: {flash:?}"
    );

    // Success stays QUIET — the OAuth card already narrates the hop.
    let mut model = launcher_model();
    open_url_effects(&mut model, "https://auth.openai.com/x", &|_| Ok(()));
    assert!(
        model.flash.is_none(),
        "a successful open must not flash over the card"
    );
}

/// `$BROWSER` wins when set (and is what the PTY probe points at a
/// recorder); the scheme allow-list refuses anything that could turn the
/// authorize hop into an arbitrary-program launch.
#[test]
fn browser_env_wins_and_the_scheme_is_allow_listed() {
    let env = |name: &str| (name == "BROWSER").then(|| "recorder-browser".to_owned());
    let command =
        open_url_command_with_env("https://auth.openai.com/x", &env).expect("http is sanctioned");
    assert_eq!(command.get_program().to_string_lossy(), "recorder-browser");
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args, vec!["https://auth.openai.com/x".to_owned()]);

    for hostile in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "ssh://evil",
        "  https://padded",
    ] {
        assert!(
            open_url_command_with_env(hostile, &env).is_err(),
            "non-http(s) must be refused before anything spawns: {hostile}"
        );
    }

    #[cfg(target_os = "macos")]
    {
        let command = open_url_command_with_env("https://auth.openai.com/x", &|_| None)
            .expect("platform default");
        assert_eq!(command.get_program().to_string_lossy(), "/usr/bin/open");
    }
}

/// MUTATION CHECK (W5f-0): revert `SessionState::busy()` to the plain
/// `badge != "IDLE"` comparison. Expected runtime failure: the errored
/// entry below answers `busy() == true` — the launcher dresses a dead
/// turn as a running one again.
/// Verified by revert on 2026-07-30.
#[test]
fn an_errored_turn_is_terminal_not_busy() {
    let entry = errored_session();
    assert!(
        !entry.busy(),
        "✗ ERRORED is a corpse, not a running session"
    );
    assert!(entry.errored(), "and the row must know it died");
}

/// MUTATION CHECK (W5f-0): drop the `errored` arm from the launcher row
/// painting in `render.rs` (let errored rows fall to the idle `●`).
/// Expected runtime failure: the frame below loses `✗` and `errored ·`.
/// Verified by revert on 2026-07-30.
#[test]
fn the_launcher_paints_an_errored_row_honestly() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.sessions.push(errored_session());
    let rows = draw_rows(&model);
    let errored_row = rows
        .iter()
        .find(|row| row.contains("errored ·"))
        .unwrap_or_else(|| panic!("an errored session must say so: {rows:#?}"));
    assert!(
        errored_row.contains("✗"),
        "the dot speaks the badge's vocabulary: {errored_row:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("running…")),
        "and nothing may claim the dead turn is running"
    );
    assert!(
        !rows.iter().any(|row| row.contains("running")),
        "the header's running counter must not count a corpse"
    );
}

/// `/sessions` opens the full browser and never presents an errored row as
/// running. The richer error label remains on the four-row launcher.
#[test]
fn the_sessions_browser_never_names_an_errored_row_running() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.sessions.push(errored_session());
    run_slash(&mut model, "/sessions");
    assert_eq!(model.screen, Screen::Sessions);
    assert!(!model.session_browser_rows()[0].busy);
    let browser = draw_rows(&model).join("\n");
    assert!(
        !browser.contains("running"),
        "the full browser must not claim it runs: {browser:?}"
    );
}

/// Owner report (v0.0.38 era): a visited-then-interrupted session wore
/// `running…` on the launcher forever — `⏸ IDLE (i)` failed the busy
/// check's string comparison against plain `IDLE`. The interrupt marker
/// is HISTORY, not activity.
/// MUTATION CHECK: restore `badge() != "IDLE"` in `SessionState::busy`.
/// Expected RUNTIME failure: the not-busy assertion below.
#[test]
fn an_interrupted_idle_session_is_not_busy_on_the_launcher() {
    let mut entry = SessionState::neutral(SessionId::new("s-idle-i"), UiGeneration::SCRATCH);
    entry.name = Some("session".to_owned());
    entry.absorb_envelope(&EventPayload::UserMessage {
        text: "hi".to_owned(),
        attachments: Vec::new(),
        mode: DeliveryMode::Steer,
    });
    assert!(
        entry.busy(),
        "a live turn IS busy — the fix must not eat it"
    );
    entry.absorb_envelope(&EventPayload::RunState(RunState::Cancelled));
    assert_eq!(entry.projection.badge(), "⏸ IDLE (i)");
    assert!(
        !entry.busy(),
        "idle(i) is TERMINAL rest — never `running…` on the launcher"
    );
}
