//! W5e-1: the OAuth add card — alias derivation, attempt gating, the demo
//! simulate path (sim confirmAuth), cancel routing, and the bottom-anchored
//! add row + hover chrome (owner asks, 2026-07-30).
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_tui::app::{AccountRow, AppEvent, AppModel, AppRequest, Hit, InputOwner, OAuthAddPhase};
use haider_tui::mock::seed_account_rows;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn accounts_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    model.accounts.apply_snapshot(seed_account_rows(), None);
    model
}

#[test]
fn oauth_receipt_flags_ambiguous_email_without_wrong_removal_hint() {
    let descriptor = |alias: &str, account_id: Option<&str>| CredentialDescriptor {
        incarnation: 0,
        credential_digest: None,

        alias: CredentialAlias::new(alias),
        provider: "anthropic-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "owner@example.test".into(),
        status: CredentialStatus::Ok,
        active: false,
        label: None,
        account_identity: Some(haider_protocol::credential::AccountIdentity {
            email: Some("owner@example.test".into()),
            display_name: None,
            account_id: account_id.map(str::to_owned),
            plan: None,
            issuer: None,
            captured_at: 1,
            verified: false,
        }),
        created_at_ms: None,
    };
    let mut model = accounts_model();
    model.accounts.apply_snapshot(
        vec![
            AccountRow::from_descriptor(&descriptor("legacy", None)),
            AccountRow::from_descriptor(&descriptor(
                "member-a",
                Some("account=a;organization=team"),
            )),
        ],
        None,
    );
    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::AnthropicOAuth,
    ));
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    model.oauth_add_completed(
        attempt,
        &descriptor("member-b", Some("account=b;organization=team")),
    );
    let message = model.accounts.message.as_deref().expect("receipt");
    assert!(message.contains("email matches distinct account IDs"));
    assert!(!message.contains("haider account remove legacy"));
    assert!(model.accounts.message_action.is_none());
    for (width, height) in [(118, 36), (80, 24)] {
        assert!(!draw(&model, width, height).contains("haider account remove legacy"));
    }
}

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

fn open_card(model: &mut AppModel) {
    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    ));
}

/// The add row + hints anchor to the BOTTOM of the screen (owner ask): even
/// with zero accounts the buttons sit just above the hints at the bottom,
/// not floating high under the empty-state line.
#[test]
fn add_row_is_bottom_anchored_even_when_empty() {
    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    model.requests.clear(); // empty rows: the live first-run state
    let frame = draw(&model, 100, 24);
    let rows: Vec<&str> = frame.lines().collect();
    let button_row = rows
        .iter()
        .position(|row| row.contains("[+ OpenAI (OAuth)]"))
        .expect("add row rendered");
    let hint_row = rows
        .iter()
        // 971 F1(c): the hint's key map degrades by width, so match the
        // stem both cuts share.
        .position(|row| row.contains("click an account"))
        .expect("hints rendered");
    // Bottom block: buttons(8, since U1 added the OpenCode Zen/Go row,
    // G4a the Ollama/LM Studio row, G4b the Azure/Bedrock/Vertex row, 940
    // split the six-button API row 4+3 to seat Haider Code, and 970 gave
    // Google Antigravity a row of its own — eight buttons do not fit one
    // 100-col line) + the shared bottom band, then status. 971 F2 (verify
    // round 1) put the key map INSIDE that band, so the three rows under
    // the grid are rule/hints/rule where they used to be blank/hints — one
    // row more, and the grid sits one row higher.
    assert!(
        button_row >= rows.len() - 12,
        "add row must sit at the bottom (row {button_row} of {})",
        rows.len()
    );
    assert!(hint_row > button_row);
}

/// MUTATION CHECK (W5e-1): make `open_oauth_add` skip the taken-alias scan
/// (always use the bare provider name). Expected runtime failure: the second
/// card below derives `openai-oauth` again instead of `openai-oauth-2`.
/// Verified by revert on 2026-07-30.
#[test]
fn oauth_alias_derives_the_smallest_free_suffix() {
    let mut model = accounts_model();
    open_card(&mut model);
    let card = model.oauth_add.as_ref().expect("card open");
    assert_eq!(card.provider, "openai-oauth");
    assert_eq!(card.alias, "openai-oauth");
    assert!(matches!(card.phase, OAuthAddPhase::Starting));
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::OAuthAddStart { provider, alias, .. }
            if provider == "openai-oauth" && alias == "openai-oauth"
    )));

    // A row already holds the bare alias → the next card takes -2.
    key(&mut model, KeyCode::Esc); // cancel the first card
    let mut rows = seed_account_rows();
    rows.push(haider_tui::app::AccountRow {
        alias: "openai-oauth".into(),
        provider: "openai".into(),
        method: AuthMethod::OAuth,
        identity: "x".into(),
        account_identity: None,
        created_at_ms: None,
        status: CredentialStatus::Ok,
        selected: false,
        base_url: None,
    });
    model.accounts.apply_snapshot(rows, None);
    open_card(&mut model);
    assert_eq!(
        model.oauth_add.as_ref().expect("second card").alias,
        "openai-oauth-2"
    );
}

/// The demo `[1]` simulate lands the account exactly like the sim's
/// confirmAuth: row added under the sim provider name, selected for its
/// provider, card closed, ✓ message.
#[test]
fn demo_simulate_lands_the_account_and_selects_it() {
    let mut model = accounts_model();
    open_card(&mut model);
    // The demo driver would answer the start; emulate its phase flip.
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    model.oauth_add_phase(
        attempt,
        OAuthAddPhase::WaitingBrowser {
            url: "http://localhost:1455/callback (demo)".into(),
            origin: "auth.openai.com".into(),
        },
    );
    key(&mut model, KeyCode::Char('1'));
    assert!(model.oauth_add.is_none(), "card closes on simulate");
    let added = model
        .accounts
        .rows
        .iter()
        .find(|row| row.alias == "openai-oauth")
        .expect("row added");
    assert!(added.selected);
    assert_eq!(added.provider, "openai");
    assert!(
        !model
            .accounts
            .rows
            .iter()
            .any(|row| row.alias == "work-chatgpt" && row.selected),
        "the provider's previous selection yields"
    );
    assert_eq!(
        model.accounts.message.as_deref(),
        Some("✓ openai → openai-oauth · oauth · active")
    );
}

/// Esc/`[2]` cancels: the card closes, the cancel request carries the
/// attempt, and a LATE phase reply for that attempt touches nothing
/// (attempt gate — the login-card law).
#[test]
fn cancel_closes_and_late_replies_are_ghosts() {
    let mut model = accounts_model();
    open_card(&mut model);
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    model.requests.clear();
    key(&mut model, KeyCode::Esc);
    assert!(model.oauth_add.is_none());
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::OAuthAddCancel { attempt: cancelled } if *cancelled == attempt
    )));
    // Late replies for the retired attempt: silent.
    model.oauth_add_phase(attempt, OAuthAddPhase::Exchanging);
    assert!(model.oauth_add.is_none());
    model.oauth_add_failed(attempt, "late failure");
    assert!(model.oauth_add.is_none());
    // A late COMPLETION for the retired attempt must not repaint either.
    let descriptor = CredentialDescriptor {
        incarnation: 0,
        credential_digest: None,
        alias: CredentialAlias::new("openai-oauth"),
        provider: "openai-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "person@example.invalid".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    let message_before = model.accounts.message.clone();
    model.oauth_add_completed(attempt, &descriptor);
    assert_eq!(model.accounts.message, message_before);
}

/// The card renders with the bottom chrome and shows the waiting copy.
#[test]
fn card_renders_waiting_copy_above_the_add_row() {
    let mut model = accounts_model();
    open_card(&mut model);
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    model.oauth_add_phase(
        attempt,
        OAuthAddPhase::WaitingBrowser {
            url: "https://auth.openai.com/x".into(),
            origin: "auth.openai.com".into(),
        },
    );
    let frame = draw(&model, 100, 30);
    assert!(frame.contains("authorize OpenAI — ChatGPT — OAuth (loopback PKCE)"));
    assert!(frame.contains("your browser opened auth.openai.com"));
    assert!(frame.contains("[1] open the link again · [2] cancel"));
    let card_row = frame
        .lines()
        .position(|row| row.contains("authorize OpenAI"))
        .expect("card row");
    let button_row = frame
        .lines()
        .position(|row| row.contains("[+ OpenAI (OAuth)]"))
        .expect("buttons");
    assert!(card_row < button_row, "card sits above the add row");
}

/// Notice size is bounded independently of alias count, provider and row health.
/// Completion through the same or another alias shares the paged notice rule.
#[test]
fn a23_duplicate_notice_has_a_bounded_summary_and_a_paged_full_list() {
    for size in [(118, 36), (80, 24)] {
        for provider in ["anthropic-oauth", "openai-oauth"] {
            for count in [2, 20, 40] {
                for long in [false, true] {
                    for health in [
                        Some(CredentialStatus::Ok),
                        Some(CredentialStatus::Expired),
                        Some(CredentialStatus::Revoked),
                        None,
                    ] {
                        for completion in ["fresh-match", "same-alias", "retarget"] {
                            check_duplicate_notice(
                                size,
                                provider,
                                count,
                                long,
                                health.clone(),
                                completion,
                            );
                        }
                    }
                }
            }
        }
    }
}

fn check_duplicate_notice(
    (width, height): (u16, u16),
    provider: &str,
    count: usize,
    long: bool,
    health: Option<CredentialStatus>,
    completion: &str,
) {
    use haider_protocol::credential::AccountIdentity;
    use haider_tui::app::AccountAddKind;
    let aliases = (0..count)
        .map(|n| {
            if long {
                format!("legacy-profile-{n:02}-{}", "a".repeat(40))
            } else {
                format!("old-{n:02}")
            }
        })
        .collect::<Vec<_>>();
    let account_id = if provider == "openai-oauth" {
        "user:1:a;workspace:4:team"
    } else {
        "account=a;organization=team"
    };
    let descriptor = |alias: &str, status| CredentialDescriptor {
        incarnation: 0,
        credential_digest: None,

        alias: CredentialAlias::new(alias),
        provider: provider.into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "same@example.test".into(),
        status,
        active: false,
        label: None,
        account_identity: Some(AccountIdentity {
            email: Some("same@example.test".into()),
            display_name: None,
            account_id: Some(account_id.into()),
            plan: None,
            issuer: None,
            captured_at: 1,
            verified: true,
        }),
        created_at_ms: None,
    };
    let saved = if long {
        format!("saved-profile-{}", "s".repeat(44))
    } else {
        "saved".into()
    };
    let requested = if completion == "same-alias" {
        saved.clone()
    } else if long {
        format!("prior-profile-{}", "x".repeat(44))
    } else {
        "requested".into()
    };
    let mut rows = vec![AccountRow::from_descriptor(&descriptor(
        &saved,
        CredentialStatus::Ok,
    ))];
    rows.extend(aliases.iter().enumerate().map(|(n, alias)| {
        AccountRow::from_descriptor(&descriptor(
            alias,
            health.clone().unwrap_or(match n % 3 {
                0 => CredentialStatus::Ok,
                1 => CredentialStatus::Expired,
                _ => CredentialStatus::Revoked,
            }),
        ))
    }));
    if completion == "retarget" {
        let mut old = descriptor(&requested, CredentialStatus::Revoked);
        old.account_identity.as_mut().expect("identity").account_id = Some(
            if provider == "openai-oauth" {
                "user:1:b;workspace:4:team"
            } else {
                "account=b;organization=team"
            }
            .into(),
        );
        rows.push(AccountRow::from_descriptor(&old));
    }
    let retained = rows.len();
    let mut model = accounts_model();
    model.accounts.apply_snapshot(rows, None);
    model.handle_hit(Hit::AccountAdd(if provider == "openai-oauth" {
        AccountAddKind::OpenAiOAuth
    } else {
        AccountAddKind::AnthropicOAuth
    }));
    let card = model.oauth_add.as_mut().expect("card");
    card.alias = requested;
    let attempt = card.attempt;
    model.oauth_add_completed(attempt, &descriptor(&saved, CredentialStatus::Ok));
    let action = format!("haider account remove {} --confirm", aliases[0]);
    let normalize = |text: String| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let frame = normalize(draw(&model, width, height));
    assert!(
        frame.contains(&action),
        "complete action at {width}x{height}/{count}/{long}/{health:?}/{completion}: {frame}"
    );
    assert!(frame.contains("F2 full notice"));
    let message = model.accounts.message.as_ref().expect("summary");
    if count > 2 {
        assert!(message.contains(&format!("and {} more", count - 2)));
        assert!(
            frame.contains(&format!("and {} more", count - 2)),
            "remaining count stays visible: {frame}"
        );
    }
    assert!(!message.contains(aliases.last().expect("last")) || count == 2);
    let cursor = model.accounts.cursor;
    key(&mut model, KeyCode::F(2));
    let first = normalize(draw(&model, width, height));
    assert_eq!(model.input_owner(), InputOwner::AccountsNotice);
    assert_eq!(
        model.painted_input().expect("painted").owner,
        InputOwner::AccountsNotice
    );
    assert!(first.contains(&aliases[0]));
    assert!(first.contains(&action));
    for _ in 0..10 {
        key(&mut model, KeyCode::PageDown);
        draw(&model, width, height);
    }
    let last = normalize(draw(&model, width, height));
    assert!(
        last.contains(aliases.last().expect("last")),
        "last alias reachable: {last}"
    );
    assert!(last.contains(&action));
    model.handle_hit(Hit::AccountRow(aliases[1].clone()));
    assert_eq!(model.accounts.cursor, cursor, "covered list is inert");
    key(&mut model, KeyCode::Home);
    assert_eq!(model.accounts.message_scroll.get(), 0);
    key(&mut model, KeyCode::Esc);
    assert!(!model.accounts.message_expanded);
    assert_eq!(
        model.accounts.rows.len(),
        retained,
        "matching and distinct saved accounts are retained"
    );
}

#[test]
fn a25_legacy_email_notice_does_not_offer_placeholder_as_a_duplicate() {
    let descriptor = |alias: &str, account_id: Option<&str>| CredentialDescriptor {
        incarnation: 0,
        credential_digest: None,

        alias: CredentialAlias::new(alias),
        provider: "anthropic-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "same@example.test".into(),
        status: CredentialStatus::Ok,
        active: false,
        label: None,
        created_at_ms: None,
        account_identity: Some(haider_protocol::credential::AccountIdentity {
            email: Some("same@example.test".into()),
            display_name: None,
            account_id: account_id.map(str::to_owned),
            plan: None,
            issuer: None,
            captured_at: 1,
            verified: false,
        }),
    };
    for incoming_placeholder in [false, true] {
        let mut model = accounts_model();
        model.accounts.apply_snapshot(
            vec![AccountRow::from_descriptor(&descriptor(
                "neighbor",
                Some("anthropic:shared"),
            ))],
            None,
        );
        model.handle_hit(Hit::AccountAdd(
            haider_tui::app::AccountAddKind::AnthropicOAuth,
        ));
        model.oauth_add.as_mut().expect("card").alias = "fresh".into();
        let attempt = model.oauth_add.as_ref().expect("card").attempt;
        model.oauth_add_completed(
            attempt,
            &descriptor("fresh", incoming_placeholder.then_some("anthropic:shared")),
        );
        let message = model.accounts.message.as_deref().expect("notice");
        assert!(!message.contains("matching aliases retained"), "{message}");
        assert!(!message.contains("updated existing"), "{message}");
        assert_eq!(
            message.contains("not deduplicated until it verifies"),
            incoming_placeholder
        );
        assert!(model.accounts.message_action.is_none());
        for (width, height) in [(118, 36), (80, 24)] {
            let text = draw(&model, width, height);
            assert!(!text.contains("haider account remove neighbor"));
        }
    }
}

#[test]
fn integrated_account_notice_retires_input_when_replaced_or_reopened() {
    for (width, height) in [(118, 36), (80, 24)] {
        let mut model = accounts_model();
        let summary = "matching aliases retained ".repeat(50);
        let details = (0..80)
            .map(|n| format!("retained alias-{n:02}\n"))
            .collect::<String>();
        let action = "haider account remove alias-00 --confirm".to_owned();
        model
            .accounts
            .set_detailed_message(summary.clone(), details.clone(), action.clone());
        draw(&model, width, height);
        key(&mut model, KeyCode::F(2));
        draw(&model, width, height);
        let opened = model.presented_input();
        assert_eq!(opened.owner, InputOwner::AccountsNotice);
        key(&mut model, KeyCode::PageDown);
        model.handle_wheel(false);
        assert!(model.accounts.message_scroll.get() > 0);
        assert_eq!(
            model.presented_input(),
            opened,
            "paging preserves the visible notice"
        );
        model.requests.clear();
        let cursor = model.accounts.cursor;
        let draft = model.composer.text().to_owned();
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
        )));
        model.handle(AppEvent::Paste("covered-input".to_owned().into()));
        model.handle_hit(Hit::AccountRow(model.accounts.rows[0].alias.clone()));
        assert!(
            model.requests.is_empty(),
            "covered input cannot request a clipboard read"
        );
        assert_eq!(model.accounts.cursor, cursor);
        assert_eq!(model.composer.text(), draft);
        model
            .accounts
            .set_detailed_message(summary, details, action);
        assert!(model.accounts.message_expanded);
        let replacement = model.presented_input();
        assert_ne!(
            replacement, opened,
            "even a repeated notice retires old input"
        );
        model.help_open = true;
        draw(&model, width, height);
        assert_eq!(model.input_owner(), InputOwner::Help);
        key(&mut model, KeyCode::Esc);
        assert!(!model.help_open);
        assert_eq!(model.presented_input(), replacement);
        draw(&model, width, height);
        key(&mut model, KeyCode::Esc);
        assert!(!model.accounts.message_expanded);
        assert_ne!(model.input_owner(), InputOwner::AccountsNotice);
        draw(&model, width, height);
        key(&mut model, KeyCode::F(2));
        assert_eq!(model.input_owner(), InputOwner::AccountsNotice);
        assert_ne!(
            model.presented_input(),
            replacement,
            "reopened notices reject old input"
        );
    }
}
