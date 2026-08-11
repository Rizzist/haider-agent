//! W5d `/accounts`: sim-parity render (tui.js:3588-3688), the
//! forbidden-optimism law (report §5.1), the revision/command gates, and Esc
//! routing (tui.js:2516-2519).
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_tui::app::{AccountRow, AppEvent, AppModel, AppRequest, PendingCacheChange, Screen};
use haider_tui::mock::{SEED_ACCOUNT_PROVIDERS, SEED_ACCOUNTS, seed_account_rows};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

fn draw(model: &AppModel, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
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
    text
}

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A model on the accounts screen with the demo seed applied — the same
/// seams the demo runtime drives (`AccountsRefresh` answered from the seed).
fn accounts_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    assert_eq!(model.screen, Screen::Accounts);
    // The reducer queued the refresh; a headless test answers it exactly
    // like the demo runtime does.
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::AccountsRefresh)),
        "entering the screen must request rows"
    );
    model.requests.clear();
    model.accounts.apply_snapshot(seed_account_rows(), None);
    model
}

fn descriptor(alias: &str, provider: &str, method: AuthMethod) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: provider.into(),
        base_url: None,
        auth_method: method,
        identity: "you@work.com · ChatGPT".into(),
        status: CredentialStatus::Ok,
        active: true,
    }
}

/// The launcher blurb's seed constants stay derived from the ONE seed list.
#[test]
fn seed_constants_match_the_seed_list() {
    let rows = seed_account_rows();
    assert_eq!(rows.len(), SEED_ACCOUNTS);
    let mut providers: Vec<&str> = rows.iter().map(|row| row.provider.as_str()).collect();
    providers.dedup();
    assert_eq!(providers.len(), SEED_ACCOUNT_PROVIDERS);
}

/// MUTATION CHECK: remove the probe-alias filter from `apply_snapshot`.
/// Expected runtime failure: the two leaked fixtures remain in the roster and
/// render beside the real Anthropic account.
#[test]
fn probe_accounts_are_absent_from_the_rendered_roster() {
    let mut model = accounts_model();
    let rows = ["probefix-api", "probe82251-api", "anthropic-oauth"]
        .into_iter()
        .map(|alias| {
            AccountRow::from_descriptor(&descriptor(alias, "anthropic-oauth", AuthMethod::OAuth))
        })
        .collect();
    assert!(model.accounts.apply_snapshot(rows, Some(2)));

    assert_eq!(
        model
            .accounts
            .rows
            .iter()
            .map(|row| row.alias.as_str())
            .collect::<Vec<_>>(),
        ["anthropic-oauth"]
    );
    let frame = draw(&model, 100, 24);
    assert!(frame.contains("anthropic-oauth"));
    assert!(!frame.contains("probefix-api"));
    assert!(!frame.contains("probe82251-api"));
}

/// MUTATION CHECK: broaden the filter to `contains("probe")`.
/// Expected runtime failure: these normal, non-fixture aliases disappear.
#[test]
fn normal_aliases_containing_probe_are_not_hidden() {
    let mut model = accounts_model();
    let aliases = ["customer-probe-work", "probe-team-api", "approbe82251-api"];
    let rows = aliases
        .into_iter()
        .map(|alias| {
            AccountRow::from_descriptor(&descriptor(alias, "anthropic", AuthMethod::ApiKey))
        })
        .collect();
    assert!(model.accounts.apply_snapshot(rows, Some(2)));
    assert_eq!(
        model
            .accounts
            .rows
            .iter()
            .map(|row| row.alias.as_str())
            .collect::<Vec<_>>(),
        aliases
    );
}

/// Sim hierarchy 1:1: head · provider groups (base URL on the group header,
/// tui.js:3596-3599) · ●/○ rows with AUTH_LABEL/identity/status/"in use" ·
/// ONE global add row after ALL groups · hints (tui.js:3684-3686).
#[test]
fn accounts_screen_renders_the_sim_hierarchy() {
    let model = accounts_model();
    let frame = draw(&model, 100, 32);

    assert!(frame.contains("ACCOUNTS — auth is harness-owned · the ADE reads this list"));
    // Group headers in first-seen provider order.
    let openai = frame.find("openai").expect("openai group");
    let anthropic = frame.find("anthropic").expect("anthropic group");
    // B6b documented divergence: the sim's third group is `google`; the
    // landed adapter's registry id is `gemini` (mock.rs seed note).
    let gemini = frame.find("gemini").expect("gemini group");
    assert!(openai < anthropic && anthropic < gemini, "sim group order");
    // The local group header carries its account's base URL.
    assert!(frame.contains("local · http://127.0.0.1:8000/v1"));
    // Selected vs sibling rows: dot + AUTH_LABEL + identity + status.
    // P1 MASK LAW: identities render MASKED by default (the U2 owner
    // addendum extended) — the raw email / key fragment never shows on
    // open; `p1_masking_sweep_tests.rs` owns the reveal laws.
    assert!(frame.contains("● work-chatgpt [oauth] · y**@w***.com · ChatGPT · active · in use"));
    assert!(frame.contains("○ billing-key [api key] · s******* · active"));
    assert!(!frame.contains("billing-key [api key] · s******* · active · in use"));
    assert!(
        !frame.contains("you@work.com") && !frame.contains("sk-…a91f"),
        "the raw identity never renders on open"
    );
    // The one global add row, after all groups (sim button order).
    let last_row = frame
        .rfind("● hf-endpoint")
        .expect("last account row present");
    let add = frame.find("[+ OpenAI (OAuth)]").expect("add row");
    assert!(add > last_row, "the add row renders AFTER all groups");
    assert!(frame.contains("[+ Custom (OpenAI-compatible)]"));
    // Hints line.
    assert!(frame.contains("click an account to make it active"));
}

/// LAW (report §5.1) — OPTIMISTIC SELECTION IS FORBIDDEN.
///
/// MUTATION CHECK: make `select_account` flip `row.selected` locally before
/// pushing the request (the sim's own useAccount behavior — the exact
/// naive-port bug this pins against). Expected runtime failure: the
/// "dot must not move before the reply" assertions below.
/// Verified by revert on 2026-07-30.
#[test]
fn selection_moves_only_on_the_correlated_reply_never_on_click() {
    let mut model = accounts_model();

    model.select_account("billing-key");
    // Request queued, gate armed…
    assert_eq!(
        model.accounts.pending_select.as_deref(),
        Some("billing-key")
    );
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::AccountSetActive { alias, .. } if alias == "billing-key"
    )));
    // …and the DOT DID NOT MOVE.
    let openai_selected: Vec<&str> = model
        .accounts
        .rows
        .iter()
        .filter(|row| row.provider == "openai" && row.selected)
        .map(|row| row.alias.as_str())
        .collect();
    assert_eq!(
        openai_selected,
        vec!["work-chatgpt"],
        "the dot must not move before the reply"
    );
    // A second click while pending is refused (one at a time).
    model.requests.clear();
    model.select_account("ci-key");
    assert!(model.requests.is_empty(), "pending select gates re-entry");

    // The correlated reply lands: NOW the dot moves, within the provider.
    model.apply_account_selected(&descriptor("billing-key", "openai", AuthMethod::ApiKey), 7);
    assert!(model.accounts.pending_select.is_none());
    let openai_selected: Vec<&str> = model
        .accounts
        .rows
        .iter()
        .filter(|row| row.provider == "openai" && row.selected)
        .map(|row| row.alias.as_str())
        .collect();
    assert_eq!(openai_selected, vec!["billing-key"]);
    assert_eq!(model.accounts.revision, Some(7));
    // Sim message format (tui.js:2166).
    assert_eq!(
        model.accounts.message.as_deref(),
        Some("✓ openai → billing-key · api key · active")
    );
    // Other providers untouched.
    assert!(
        model
            .accounts
            .rows
            .iter()
            .any(|row| row.alias == "personal-max" && row.selected)
    );
}

/// CM3 account switches use the same exact-repeat confirmation as model and
/// tuning changes; the warning releases the pending row without moving it.
#[test]
fn cm3_account_switch_repeats_with_new_epoch_confirmation() {
    let mut model = accounts_model();
    model.select_account("billing-key");
    assert!(matches!(
        model.requests.last(),
        Some(AppRequest::AccountSetActive {
            alias,
            confirm_new_epoch: false,
        }) if alias == "billing-key"
    ));

    model.requests.clear();
    model.cache_epoch_confirmation_required(
        PendingCacheChange::Account {
            alias: "billing-key".into(),
        },
        "account/auth; 1000 stable-prefix tokens invalidated; plan",
    );
    assert!(model.accounts.pending_select.is_none());
    model.select_account("billing-key");
    assert!(matches!(
        model.requests.last(),
        Some(AppRequest::AccountSetActive {
            alias,
            confirm_new_epoch: true,
        }) if alias == "billing-key"
    ));
}

/// LAW — the revision gate: a LATE result (older revision) may clear its
/// pending gate but must not rewrite newer rows.
///
/// MUTATION CHECK: drop the `revision < current` comparison in
/// `apply_account_selected` (apply unconditionally). Expected runtime
/// failure: the stale reply below moves the dot back to `billing-key`.
/// Verified by revert on 2026-07-30.
#[test]
fn a_stale_set_active_result_cannot_regress_newer_rows() {
    let mut model = accounts_model();
    // The screen has already seen revision 10 (e.g. a newer snapshot won
    // while our select was in flight).
    model.accounts.apply_snapshot(seed_account_rows(), Some(10));

    model.select_account("billing-key");
    // A LATE result from an older revision epoch arrives.
    model.apply_account_selected(&descriptor("billing-key", "openai", AuthMethod::ApiKey), 3);
    // Gate released…
    assert!(model.accounts.pending_select.is_none());
    // …but the rows did NOT regress.
    let openai_selected: Vec<&str> = model
        .accounts
        .rows
        .iter()
        .filter(|row| row.provider == "openai" && row.selected)
        .map(|row| row.alias.as_str())
        .collect();
    assert_eq!(
        openai_selected,
        vec!["work-chatgpt"],
        "a stale reply must not rewrite newer rows"
    );
    assert_eq!(model.accounts.revision, Some(10));

    // Same gate on snapshots: an older account.list cannot regress either.
    let mut stale_rows = seed_account_rows();
    for row in &mut stale_rows {
        row.selected = false;
    }
    assert!(!model.accounts.apply_snapshot(stale_rows, Some(2)));
    assert!(
        model
            .accounts
            .rows
            .iter()
            .any(|row| row.alias == "work-chatgpt" && row.selected)
    );
}

/// An unusable row (additive W5 vocabulary) is refused locally with an
/// honest message — no doomed RPC, no dot movement.
#[test]
fn expired_and_revoked_rows_refuse_selection_locally() {
    let mut model = accounts_model();
    let mut rows = seed_account_rows();
    rows.iter_mut()
        .find(|row| row.alias == "billing-key")
        .expect("seed row")
        .status = CredentialStatus::Expired;
    model.accounts.apply_snapshot(rows, None);

    model.requests.clear();
    model.select_account("billing-key");
    assert!(model.requests.is_empty(), "no RPC for an unusable row");
    assert!(model.accounts.pending_select.is_none());
    assert_eq!(
        model.accounts.message.as_deref(),
        Some("· billing-key is not usable — /login to re-authenticate")
    );
    // And it renders the additive status vocabulary (identity MASKED —
    // the P1 default).
    let frame = draw(&model, 100, 32);
    assert!(frame.contains("○ billing-key [api key] · s******* · expired"));
}

/// Esc routing (sim tui.js:2516-2519): no session → launcher; the sim's
/// re-click parity: clicking the ACTIVE row re-emits the message without a
/// daemon round-trip (useAccount has no early return, tui.js:2160-2168).
#[test]
fn esc_returns_to_launcher_and_reclick_reemits_the_message() {
    let mut model = accounts_model();

    model.requests.clear();
    model.select_account("work-chatgpt");
    assert!(model.requests.is_empty(), "active row: no RPC");
    assert_eq!(
        model.accounts.message.as_deref(),
        Some("✓ openai → work-chatgpt · oauth · active")
    );

    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Launcher);
}

/// The pending row shows in-flight feedback (…) without moving the dot.
#[test]
fn pending_select_renders_feedback_without_moving_the_dot() {
    let mut model = accounts_model();
    model.select_account("billing-key");
    let frame = draw(&model, 100, 32);
    // Identity MASKED (the P1 default) — the pending pulse rides the row.
    assert!(frame.contains("○ billing-key [api key] · s******* · active …"));
    assert!(frame.contains("● work-chatgpt"));
}
