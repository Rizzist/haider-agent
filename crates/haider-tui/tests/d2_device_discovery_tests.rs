//! D2 — the "found on this device" section: D1's daemon-owned device
//! credential discovery surfaced on `/accounts` and the shared `/providers`
//! buttons area.
//!
//! The laws:
//! * the section lists provider · source label · freshness per candidate,
//!   numbered for one-key import — and ONLY when the live daemon reported
//!   candidates;
//! * a digit/⏎/click dispatches the receipted `account.import_device`
//!   through the outbox and installs NOTHING locally — the chained
//!   `account.list` refresh is the only materializer;
//! * `import_supported:false` rows are dim, carry their honest reason, and
//!   are inert (no number, no hit, no dispatch — even from a forged
//!   coordinate);
//! * an ungated daemon hides the section entirely and is never asked;
//! * demo is honest: the sim has no device to probe, so the section is
//!   absent and the discovery vocabulary unreachable.
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_rpc::DeviceCredentialCandidateWire;
use haider_tui::app::{AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use haider_tui::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

fn key(code: KeyCode) -> haider_tui::app::AppEvent {
    haider_tui::app::AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A live model with the given daemon feature set and the seed
/// account/provider snapshots applied (boot always issues `account.list` +
/// `provider.list`, so a connected TUI holds both before any screen can be
/// clicked).
fn live_model(features: &[&str]) -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = features.iter().map(|name| (*name).to_owned()).collect();
    model.daemon_version = Some("0.0.66".to_owned());
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model
}

/// The discovery-gated live model, walked onto `/accounts`, with the
/// daemon's candidate report applied — two importable stores and one
/// honest refusal, the D1 fixture shape.
fn discovery_model() -> (AppModel, LiveDriver) {
    let mut model = live_model(&[haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1]);
    let mut driver = LiveDriver::new("test");
    run_slash(&mut model, "/accounts");
    // Entry issues the read; the daemon answers with the fixture report.
    let issued = live_pass(&mut driver, &mut model, None, std::time::Instant::now()).commands;
    assert!(
        issued.contains(&LiveCommand::DeviceCandidates),
        "screen entry asks for the report: {issued:?}"
    );
    driver.apply(
        &mut model,
        LiveReply::DeviceCandidates {
            discovery_disabled: false,
            candidates: fixture_candidates(),
        },
    );
    (model, driver)
}

fn fixture_candidates() -> Vec<DeviceCredentialCandidateWire> {
    vec![
        DeviceCredentialCandidateWire {
            candidate: "dev-codex-1".to_owned(),
            provider: "openai".to_owned(),
            source_label: "Codex CLI".to_owned(),
            account_label: Some("you@work.com".to_owned()),
            freshness: "fresh".to_owned(),
            expires_at_ms: Some(1_999_999),
            path: "~/.codex/auth.json".to_owned(),
            import_supported: true,
            unsupported_reason: None,
        },
        DeviceCredentialCandidateWire {
            candidate: "dev-kimi-1".to_owned(),
            provider: "kimi-oauth".to_owned(),
            source_label: "Kimi Code".to_owned(),
            account_label: None,
            freshness: "expiring".to_owned(),
            expires_at_ms: None,
            path: "~/.kimi/credentials/kimi-code.json".to_owned(),
            import_supported: false,
            unsupported_reason: None,
        },
        DeviceCredentialCandidateWire {
            candidate: "dev-gemini-1".to_owned(),
            provider: "gemini".to_owned(),
            source_label: "Gemini CLI".to_owned(),
            account_label: None,
            freshness: "unknown".to_owned(),
            expires_at_ms: None,
            path: "~/.gemini/oauth_creds.json".to_owned(),
            import_supported: false,
            unsupported_reason: Some(
                "bundle shape unverified — sign in from the buttons below".to_owned(),
            ),
        },
    ]
}

/// Two supported stores instead — for the numbering/second-digit laws.
fn two_supported() -> Vec<DeviceCredentialCandidateWire> {
    let mut candidates = fixture_candidates();
    candidates[1].import_supported = true;
    candidates[1].unsupported_reason = None;
    candidates
}

fn draw(
    model: &AppModel,
    width: u16,
    height: u16,
) -> (
    String,
    Vec<(ratatui::layout::Rect, Hit)>,
    ratatui::buffer::Buffer,
) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| {
            hits = render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    (text, hits, buffer)
}

/// The (x, y) cell coordinate where `needle` starts in the drawn text.
fn locate(text: &str, needle: &str) -> (u16, u16) {
    for (y, line) in text.lines().enumerate() {
        if let Some(byte) = line.find(needle) {
            let x = line[..byte].chars().count();
            return (x as u16, y as u16);
        }
    }
    panic!("`{needle}` not on screen:\n{text}");
}

fn imported_descriptor() -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new("codex-cli"),
        provider: "openai".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "you@work.com · ChatGPT".into(),
        status: CredentialStatus::Ok,
        active: false,
    }
}

// ------------------------------------------------------------- the laws --

/// LAW — the section lists each discovered candidate with its provider,
/// source label, and freshness hint (plus the account label when the store
/// itself carried one), numbered for one-key import, on BOTH screens that
/// share the buttons area.
///
/// MUTATION CHECK: drop the freshness span from
/// `push_device_candidates_section` (render the row without the hint).
/// Expected RUNTIME failure: the `· fresh` /` · expiring` assertions below.
/// Verified by revert on 2026-08-05.
#[test]
fn device_section_lists_candidates_with_freshness() {
    let (mut model, _driver) = discovery_model();
    let (text, _, _) = draw(&model, 118, 40);
    assert!(
        text.contains("found on this device"),
        "the section header renders:\n{text}"
    );
    // P1 MASK LAW: the account label is a real identity (the store's
    // signed-in email) — MASKED by default; `p1_masking_sweep_tests.rs`
    // owns the reveal laws.
    assert!(
        text.contains("[1] Codex CLI · openai · y**@w***.com · fresh"),
        "supported row: number · source · provider · MASKED account label · freshness:\n{text}"
    );
    assert!(
        !text.contains("you@work.com"),
        "the raw account label never renders on open:\n{text}"
    );
    assert!(
        text.contains("Kimi Code · kimi-oauth · expiring"),
        "every candidate carries source · provider · freshness:\n{text}"
    );
    assert!(
        text.contains("Gemini CLI · gemini · unknown"),
        "the unknown freshness hint is honest, not invented:\n{text}"
    );

    // The shared /providers buttons area carries the SAME section. The
    // accounts screen has no composer, so the walk goes through the
    // launcher (esc) like a user's would.
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/providers");
    assert_eq!(model.screen, Screen::Providers);
    // G4b widened the pinned footer (a sixth button row + the enterprise
    // hint line), so the same walk needs two more rows to keep the device
    // section on screen.
    let (text, _, _) = draw(&model, 118, 46);
    assert!(
        text.contains("found on this device") && text.contains("[1] Codex CLI · openai"),
        "the providers buttons area shares the section:\n{text}"
    );
}

/// LAW — a digit (and ⏎ on the highlighted candidate, and a click on the
/// row's hit) dispatches ONE receipted `account.import_device` through the
/// outbox, carrying only the opaque candidate id — and installs NOTHING
/// locally: the account row appears only when the chained `account.list`
/// refresh applies.
///
/// MUTATION CHECK: in the live driver's `AppRequest::DeviceImport` arm,
/// return the command WITHOUT `self.enqueue(..)` (dispatch without the
/// outbox). Expected RUNTIME failure: the `outbox_len() == 1` assertion.
/// Verified by revert on 2026-08-05.
#[test]
fn one_key_import_dispatches_receipted_command_and_installs_nothing_locally() {
    let (mut model, mut driver) = discovery_model();
    let rows_before = model.accounts.rows.clone();

    model.handle(key(KeyCode::Char('1')));
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::DeviceImport { candidate } if candidate == "dev-codex-1"
        )),
        "the digit dispatches the import request: {:?}",
        model.requests
    );
    let issued = live_pass(&mut driver, &mut model, None, std::time::Instant::now()).commands;
    let Some(LiveCommand::DeviceImport {
        command_id,
        candidate,
    }) = issued
        .iter()
        .find(|command| matches!(command, LiveCommand::DeviceImport { .. }))
        .cloned()
    else {
        panic!("the wire command rides: {issued:?}");
    };
    assert_eq!(candidate, "dev-codex-1", "only the opaque id crosses");
    assert_eq!(
        driver.outbox_len(),
        1,
        "receipted + durable: the import waits in the outbox"
    );
    assert_eq!(
        model.accounts.rows, rows_before,
        "nothing installed at dispatch"
    );
    assert_eq!(
        model.device.pending_import.as_deref(),
        Some("dev-codex-1"),
        "the pending gate holds the exact candidate"
    );
    let (text, _, _) = draw(&model, 118, 40);
    assert!(
        text.contains("importing…"),
        "in-flight feedback without a row move:\n{text}"
    );

    // A second digit while one is in flight is refused honestly.
    model.handle(key(KeyCode::Char('1')));
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::DeviceImport { .. })),
        "one import at a time: {:?}",
        model.requests
    );
    model.requests.clear();

    // The receipt retires the outbox entry, names the commit, and chains
    // the refresh — it still installs NOTHING itself.
    let followups = driver.apply(
        &mut model,
        LiveReply::DeviceImported {
            command_id,
            descriptor: imported_descriptor(),
            revision: 2,
        },
    );
    assert!(
        followups.contains(&LiveCommand::AccountList)
            && followups.contains(&LiveCommand::ProviderList),
        "success lands via the normal account.list refresh: {followups:?}"
    );
    assert_eq!(driver.outbox_len(), 0, "the receipt retires the entry");
    assert_eq!(
        model.accounts.rows, rows_before,
        "the receipt installs nothing — daemon truth only"
    );
    assert!(model.device.pending_import.is_none(), "the gate releases");
    let message = model
        .device
        .message
        .as_deref()
        .expect("the commit is named");
    assert!(
        message.contains("imported openai → codex-cli"),
        "the receipt names what the daemon committed: {message}"
    );
    // P1 MASK LAW: the receipt carries the identity MASKED-ALWAYS —
    // receipts are transient chrome with no reveal loop of their own.
    assert!(
        message.contains("y**@w***.com") && !message.contains("you@work.com"),
        "the receipt masks the identity: {message}"
    );

    // The chained snapshot is the materializer.
    let mut descriptors = vec![imported_descriptor()];
    descriptors[0].active = true;
    driver.apply(
        &mut model,
        LiveReply::Accounts {
            descriptors,
            revision: Some(2),
        },
    );
    assert!(
        model
            .accounts
            .rows
            .iter()
            .any(|row| row.alias == "codex-cli"),
        "the account appears when account.list truth applies"
    );
}

/// LAW — `import_supported:false` rows render DIM with their honest
/// reason, unnumbered, and are inert: no hit rect exists for them, a
/// forged click coordinate dispatches nothing, and no digit reaches them
/// (numbering skips them by construction).
///
/// MUTATION CHECK: in `import_device_candidate`, delete the
/// `!candidate.import_supported` early return. Expected RUNTIME failure:
/// the forged-hit assertion below (a `DeviceImport` request rides).
/// Verified by revert on 2026-08-05.
#[test]
fn unsupported_rows_are_dim_honest_and_inert() {
    let (mut model, _driver) = discovery_model();
    let (text, hits, buffer) = draw(&model, 118, 40);

    // Honest: the refusal reason rides the row; a reasonless refusal still
    // says it is not supported.
    assert!(
        text.contains("Gemini CLI · gemini · unknown — bundle shape unverified"),
        "the honest reason renders on the row:\n{text}"
    );
    assert!(
        text.contains("Kimi Code · kimi-oauth · expiring — import not supported"),
        "a missing reason degrades to the honest default:\n{text}"
    );
    // Unnumbered: exactly one supported candidate, so `[2]`/`[3]` never
    // render in the section.
    assert!(
        !text.contains("[2] "),
        "unsupported rows take no number:\n{text}"
    );

    // Dim: the unsupported label wears the theme's dim ink while the
    // supported label wears the bright ink.
    let theme = model.theme.theme();
    let (x, y) = locate(&text, "Gemini CLI");
    assert_eq!(
        buffer[(x, y)].style().fg,
        theme.dim_style().fg,
        "unsupported rows are dim"
    );
    let (x, y) = locate(&text, "Codex CLI");
    assert_eq!(
        buffer[(x, y)].style().fg,
        theme.bright_style().fg,
        "supported rows keep the bright label"
    );

    // Inert: no hit rect exists for either unsupported candidate…
    assert!(
        !hits.iter().any(|(_, hit)| matches!(
            hit,
            Hit::DeviceImport(id) if id == "dev-gemini-1" || id == "dev-kimi-1"
        )),
        "unsupported rows are not clickable-to-import"
    );
    // …and even a forged coordinate dispatches nothing.
    model.requests.clear();
    model.handle_hit(Hit::DeviceImport("dev-gemini-1".to_owned()));
    assert!(
        model.requests.is_empty() && model.device.pending_import.is_none(),
        "a forged unsupported coordinate is inert: {:?}",
        model.requests
    );
    // No digit reaches them: `2` names no supported candidate.
    model.handle(key(KeyCode::Char('2')));
    assert!(
        model.requests.is_empty(),
        "digits past the supported count are inert: {:?}",
        model.requests
    );
}

/// LAW — a daemon that does not advertise
/// `account_device_discovery_v1` is never asked, and the section is
/// absent without a notice (discovery is an enhancement).
///
/// MUTATION CHECK: in `enter_accounts`, push
/// `AppRequest::DeviceCandidatesRefresh` unconditionally (drop the
/// `device_discovery_available` gate). Expected RUNTIME failure: the
/// no-request assertion below.
/// Verified by revert on 2026-08-05.
#[test]
fn ungated_daemon_hides_the_section() {
    let mut model = live_model(&[]);
    let mut driver = LiveDriver::new("test");
    run_slash(&mut model, "/accounts");
    assert_eq!(model.screen, Screen::Accounts);
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::DeviceCandidatesRefresh)),
        "an ungated daemon is never asked: {:?}",
        model.requests
    );
    let issued = live_pass(&mut driver, &mut model, None, std::time::Instant::now()).commands;
    assert!(
        !issued.contains(&LiveCommand::DeviceCandidates),
        "no wire read either: {issued:?}"
    );
    let (text, _, _) = draw(&model, 118, 40);
    assert!(
        !text.contains("found on this device"),
        "the section is absent, no notice needed:\n{text}"
    );
}

/// LAW — demo is honest: the sim has no device to probe, so the section is
/// absent, the read is never pushed, and the import vocabulary is
/// unreachable (digits fall through to nothing).
///
/// MUTATION CHECK: make `device_discovery_available` delegate to
/// `daemon_serves` (which is demo-true). Expected RUNTIME failure: the
/// no-request assertion below — demo entry pushes the discovery read.
/// Verified by revert on 2026-08-05.
#[test]
fn demo_is_honest() {
    let mut model = launcher_model();
    assert_eq!(model.mode, RuntimeMode::Demo);
    run_slash(&mut model, "/accounts");
    assert_eq!(model.screen, Screen::Accounts);
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::DeviceCandidatesRefresh)),
        "demo never asks a daemon it does not have: {:?}",
        model.requests
    );
    model.requests.clear();
    model.accounts.apply_snapshot(seed_account_rows(), None);
    let (text, _, _) = draw(&model, 118, 40);
    assert!(
        !text.contains("found on this device"),
        "sim-honest: no fabricated discovery section:\n{text}"
    );
    // The import vocabulary is unreachable: a digit dispatches nothing.
    model.handle(key(KeyCode::Char('1')));
    assert!(
        model.requests.is_empty(),
        "no import can ride in demo: {:?}",
        model.requests
    );
}

// ------------------------------------------------- the supporting seams --

/// The wire shapes: the read maps to `account.device_candidates`, the
/// import to `account.import_device` (command id + opaque candidate id,
/// nothing else), and both responses map back through the link.
///
/// MUTATION CHECK: map `LiveCommand::DeviceCandidates` to
/// `RequestBody::AccountList { provider: None }` (a plausible copy-paste).
/// Expected RUNTIME failure: the read's request-body assertion.
/// Verified by revert on 2026-08-05.
#[test]
fn the_wire_shapes_round_trip() {
    use haider_rpc::{CommandId, RequestBody, ResponseBody};
    assert!(matches!(
        request_body(LiveCommand::DeviceCandidates),
        RequestBody::AccountDeviceCandidates
    ));
    let command = LiveCommand::DeviceImport {
        command_id: CommandId::new("cmd-import-1"),
        candidate: "dev-codex-1".to_owned(),
    };
    let context = CommandContext::of(&command);
    assert!(matches!(
        request_body(command),
        RequestBody::AccountImportDevice { command_id, candidate }
            if command_id.as_str() == "cmd-import-1" && candidate == "dev-codex-1"
    ));

    let replies = map_response(
        &CommandContext::of(&LiveCommand::DeviceCandidates),
        ResponseBody::AccountDeviceCandidates {
            discovery_disabled: true,
            candidates: fixture_candidates(),
        },
    );
    assert!(
        matches!(
            replies.as_slice(),
            [LiveReply::DeviceCandidates {
                discovery_disabled: true,
                candidates,
            }] if candidates.len() == 3
        ),
        "the report maps whole, disabled state included: {replies:?}"
    );
    let replies = map_response(
        &context,
        ResponseBody::AccountImportDevice {
            descriptor: imported_descriptor(),
            revision: 4,
        },
    );
    assert!(
        matches!(
            replies.as_slice(),
            [LiveReply::DeviceImported {
                command_id,
                revision: 4,
                ..
            }] if command_id.as_str() == "cmd-import-1"
        ),
        "the receipt correlates by the issuing command id: {replies:?}"
    );
}

/// A failed import releases the exact pending gate and lands the daemon's
/// typed reason inside the section — nothing moved, so nothing rolls back,
/// and the next attempt can dispatch.
///
/// MUTATION CHECK: in the driver's `Failed` arm for
/// `pending_device_import`, drop `model.device.pending_import = None`.
/// Expected RUNTIME failure: the gate-release assertion below (the retry
/// is refused as "one import at a time").
/// Verified by revert on 2026-08-05.
#[test]
fn a_failed_import_releases_the_gate_with_the_honest_reason() {
    let (mut model, mut driver) = discovery_model();
    model.handle(key(KeyCode::Char('1')));
    let issued = live_pass(&mut driver, &mut model, None, std::time::Instant::now()).commands;
    let Some(LiveCommand::DeviceImport { command_id, .. }) = issued
        .iter()
        .find(|command| matches!(command, LiveCommand::DeviceImport { .. }))
        .cloned()
    else {
        panic!("the import rides: {issued:?}");
    };
    driver.apply(
        &mut model,
        LiveReply::Failed {
            command_id: Some(command_id),
            code: "import_failed".to_owned(),
            message: "the store changed under the read — re-open /accounts".to_owned(),
            retryable: false,
        },
    );
    assert!(model.device.pending_import.is_none(), "the gate releases");
    assert_eq!(driver.outbox_len(), 0, "a terminal failure retires it");
    let message = model.device.message.as_deref().expect("the reason lands");
    assert!(
        message.contains("import failed — the store changed under the read"),
        "the daemon's honest reason, verbatim: {message}"
    );
    // The next attempt is free to dispatch.
    model.requests.clear();
    model.handle(key(KeyCode::Char('1')));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::DeviceImport { .. })),
        "a released gate accepts the retry: {:?}",
        model.requests
    );
}

/// The read rides SCREEN ENTRY only: `/providers` entry asks too (the
/// section is shared), re-entry asks again, and neither the accounts
/// refresh nor the report's own application chains another read — no
/// polling exists to kill.
///
/// MUTATION CHECK: in the driver's `AppRequest::AccountsRefresh` arm, also
/// return `LiveCommand::DeviceCandidates` (a plausible "keep it fresh").
/// Expected RUNTIME failure: the accounts-refresh isolation assertion.
/// Verified by revert on 2026-08-05.
#[test]
fn the_candidates_read_rides_screen_entry_only() {
    let (mut model, mut driver) = discovery_model();
    // The report's application chains nothing.
    let followups = driver.apply(
        &mut model,
        LiveReply::DeviceCandidates {
            discovery_disabled: false,
            candidates: two_supported(),
        },
    );
    assert!(followups.is_empty(), "no chained re-read: {followups:?}");
    // An accounts refresh alone never re-reads the device.
    model.requests.push(AppRequest::AccountsRefresh);
    let issued = live_pass(&mut driver, &mut model, None, std::time::Instant::now()).commands;
    assert!(
        !issued.contains(&LiveCommand::DeviceCandidates),
        "account.list truth does not poll discovery: {issued:?}"
    );
    // Leaving and re-entering asks again — entry is the ONE refresh door.
    model.handle(key(KeyCode::Esc));
    assert_ne!(model.screen, Screen::Accounts);
    run_slash(&mut model, "/accounts");
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::DeviceCandidatesRefresh)),
        "re-entry refreshes: {:?}",
        model.requests
    );
    model.requests.clear();
    // The shared /providers area is an entry door of its own (walked via
    // the launcher — the accounts screen has no composer).
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/providers");
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::DeviceCandidatesRefresh)),
        "/providers entry refreshes the shared section: {:?}",
        model.requests
    );
}

/// ⏎ on the highlighted candidate row imports it (the owner menu law's
/// other half): ↓ walks past the account rows into the numbered
/// candidates, and Enter dispatches the exact highlighted id.
///
/// MUTATION CHECK: in the accounts key handler's `Down` arm, clamp to
/// `rows.len() - 1` (drop the supported-candidate extension). Expected
/// RUNTIME failure: Enter below selects an account instead of importing.
/// Verified by revert on 2026-08-05.
#[test]
fn enter_on_the_highlighted_candidate_imports_it() {
    let (mut model, mut driver) = discovery_model();
    driver.apply(
        &mut model,
        LiveReply::DeviceCandidates {
            discovery_disabled: false,
            candidates: two_supported(),
        },
    );
    // Walk the cursor past every account row onto the SECOND candidate.
    let steps = model.accounts.rows.len() + 1;
    for _ in 0..steps {
        model.handle(key(KeyCode::Down));
    }
    assert_eq!(
        model.accounts.cursor,
        model.accounts.rows.len() + 1,
        "the flattened selectable rows extend into the candidates"
    );
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::DeviceImport { candidate } if candidate == "dev-kimi-1"
        )),
        "⏎ imports the highlighted candidate: {:?}",
        model.requests
    );
    assert!(
        model.accounts.pending_select.is_none(),
        "no account select rode the same key"
    );
}
