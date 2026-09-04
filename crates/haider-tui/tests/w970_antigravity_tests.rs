//! v0.0.970 Google Antigravity — the `/login google` door, the first-login
//! disclosure and its journalled acknowledgement, the standing `/accounts`
//! terms badge, and the honest (unavailable, never zero) meter.
//!
//! The provider ships ENABLED BY DEFAULT with no policy gate (owner decision
//! 2026-09-03), so the disclosure IS the safeguard: these pins hold the
//! warning text to the byte, hold the acknowledgement to exactly one durable
//! record, and hold every rendered surface to showing no OAuth URL, query,
//! code or token.
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, LocalUsageStatsV1, UsageReportV1, UsageWindowV1,
};
use haider_tui::app::{
    AccountAddKind, AccountRow, AccountSourceRow, AppModel, AppRequest,
    GOOGLE_ANTIGRAVITY_PROVIDER, GOOGLE_ANTIGRAVITY_SOURCE_KIND, GOOGLE_ANTIGRAVITY_TERMS_SUBJECT,
    GOOGLE_ANTIGRAVITY_TERMS_WARNING, OAuthAddPhase, Screen, UsageScope,
};
use haider_tui::runtime::sync_terms_persistence;
use haider_tui::terms_journal::{TERMS_JOURNAL_FILE, TermsJournal};
use ratatui::crossterm::event::KeyCode;

mod common;
mod tuivirt_common;
use common::{key, run_slash};
use tuivirt_common::{SIZES, Snapshot, check_golden, draw, launcher_model};

/// Friday 04 September 2026, 12:00 UTC — the `oauth_calendar_tests` epoch, so
/// both calendars talk about the same month.
const GENERATED_AT_MS: u64 = 1_788_523_200_000;

/// The machine-readable reason the daemon reports for a Google account: ACP
/// exposes no structured quota at all (`_acp-wire-facts.md`), so there is
/// nothing to meter and nothing to guess a reset from.
const NO_QUOTA_REASON: &str = "acp_agent_publishes_no_quota";

// ---- helpers ----------------------------------------------------------

fn accounts_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    assert_eq!(model.screen, Screen::Accounts);
    model.requests.clear();
    model
}

fn descriptor(alias: &str, provider: &str, identity: &str) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: provider.into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: identity.into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}

/// The accounts screen with exactly one Google account on it.
fn google_accounts_model() -> AppModel {
    let mut model = accounts_model();
    model.accounts.apply_snapshot(
        vec![AccountRow::from_descriptor(&descriptor(
            "google-antigravity",
            GOOGLE_ANTIGRAVITY_PROVIDER,
            "pilot@example.com",
        ))],
        Some(7),
    );
    model
}

/// A model parked on the disclosure card, straight through `/login google`.
fn disclosure_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/login google");
    model
}

/// The rendered terms warning, unwrapped back into one line: the rows after
/// the `terms warning` marker, up to the blank line or the key map that ends
/// the block. Joining the trimmed rows with one space reverses the wrap
/// exactly, because `wrap_body` only ever breaks at a space run.
fn rendered_warning(frame: &Snapshot) -> String {
    let first = frame
        .row_containing("terms warning")
        .expect("frame has a terms-warning marker");
    frame.rows[first + 1..]
        .iter()
        .map(|row| row.trim().to_owned())
        .take_while(|row| !row.is_empty() && !row.starts_with('['))
        .collect::<Vec<_>>()
        .join(" ")
}

fn account_report(provider: &str, alias: &str, meter: AccountMeterStateV1) -> AccountUsageReportV1 {
    AccountUsageReportV1 {
        provider: provider.to_owned(),
        alias: CredentialAlias::new(alias),
        identity: Some("pilot@example.com".to_owned()),
        plan: None,
        auth_method: AuthMethod::OAuth,
        meter,
        local: LocalUsageStatsV1::default(),
    }
}

/// One unlabeled five-hour window resetting Fri 04 Sep 2026 16:42 UTC — the
/// `oauth_calendar_tests` fixture instant.
fn five_hour_window() -> UsageWindowV1 {
    UsageWindowV1 {
        window: "five_hour".into(),
        utilization: 0.5,
        resets_at_ms: Some(1_788_540_120_000),
        label: None,
    }
}

fn usage_model_from(accounts: Vec<AccountUsageReportV1>, scope: UsageScope) -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/usage");
    assert_eq!(model.screen, Screen::Usage);
    model.usage.apply_report(UsageReportV1 {
        generated_at_ms: GENERATED_AT_MS,
        accounts,
    });
    // The `s` ring has five scopes; bounded so a ring change fails loudly
    // instead of hanging the suite.
    for _ in 0..5 {
        if model.usage.scope == scope {
            return model;
        }
        model.handle(key(KeyCode::Char('s')));
    }
    assert_eq!(model.usage.scope, scope, "the scope ring reached {scope:?}");
    model
}

fn usage_model(meter: AccountMeterStateV1, scope: UsageScope) -> AppModel {
    usage_model_from(
        vec![account_report(
            GOOGLE_ANTIGRAVITY_PROVIDER,
            "google-antigravity",
            meter,
        )],
        scope,
    )
}

// ---- 1. the /login door ------------------------------------------------

/// `/login google` is the preferred shortcut and needs no method word: it
/// lands on `/accounts` with the disclosure open and NOTHING started.
///
/// MUTATION CHECK: drop the `("google" | GOOGLE_ANTIGRAVITY_PROVIDER, "" |
/// "oauth")` arm from the `"login"` command. Expected runtime failure:
/// `/login google` falls through to the `(provider, "")` slot arm, parking
/// `/login google ` in the composer instead of opening the card.
#[test]
fn login_google_opens_the_disclosure_and_starts_nothing() {
    let model = disclosure_model();
    assert_eq!(model.screen, Screen::Accounts);
    assert_eq!(
        model.antigravity_consent,
        Some(AccountAddKind::GoogleAntigravity)
    );
    assert!(model.oauth_add.is_none(), "no flow has started");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::OAuthAddStart { .. })),
        "nothing is downloaded or authorized before the user confirms"
    );
}

/// The explicit existing grammar reaches the SAME door — one flow, two
/// spellings, never two implementations.
#[test]
fn login_google_antigravity_oauth_reaches_the_same_door() {
    let mut model = launcher_model();
    run_slash(&mut model, "/login google-antigravity oauth");
    assert_eq!(model.screen, Screen::Accounts);
    assert_eq!(
        model.antigravity_consent,
        Some(AccountAddKind::GoogleAntigravity)
    );
}

/// Every API-key spelling is refused, and the refusal names the account a
/// user asking for one probably wants: `gemini` is a separate credential.
///
/// MUTATION CHECK: delete the `("google" | GOOGLE_ANTIGRAVITY_PROVIDER, _)`
/// refusal arm. Expected runtime failure: `/login google api` reaches the
/// generic `(provider, "api")` arm and opens a masked API-key card for a
/// provider that has no API-key route at all.
#[test]
fn login_google_api_is_refused_and_names_the_separate_gemini_account() {
    for line in ["/login google api", "/login google-antigravity api"] {
        let mut model = launcher_model();
        run_slash(&mut model, line);
        let flash = model.flash.clone().unwrap_or_default();
        assert!(
            flash.contains("agent-owned OAuth only"),
            "{line}: the refusal says why: {flash}"
        );
        assert!(
            flash.contains("/login gemini api"),
            "{line}: the refusal names the separate API-key account: {flash}"
        );
        assert!(
            model.antigravity_consent.is_none(),
            "{line}: nothing opened"
        );
        assert!(model.login.is_none(), "{line}: no API-key card opened");
        assert!(model.oauth_add.is_none(), "{line}: no flow started");
    }
}

// ---- 2. the disclosure's contract --------------------------------------

/// LAW — the terms warning renders VERBATIM at every owner width, in both
/// themes. It is wrapped, never reworded and never truncated.
///
/// MUTATION CHECK: reword one clause of `GOOGLE_ANTIGRAVITY_TERMS_WARNING`'s
/// rendering (e.g. drop "reportedly"). Expected runtime failure: the joined
/// rows stop equalling the constant.
#[test]
fn the_disclosure_shows_the_terms_warning_verbatim_at_every_width() {
    let model = disclosure_model();
    for (width, height) in SIZES {
        let frame = draw(&model, width, height);
        assert_eq!(
            rendered_warning(&frame),
            GOOGLE_ANTIGRAVITY_TERMS_WARNING,
            "the warning survives wrapping at {width}x{height}"
        );
    }
}

/// Before anything starts the card states WHO signs in, WHAT the agent is,
/// and what it COSTS — every figure first-hand from `_antigravity-pins.md`.
///
/// MUTATION CHECK: drop `GOOGLE_ANTIGRAVITY_COST_LINES` from
/// `antigravity_consent_lines`. Expected runtime failure: the download and
/// footprint assertions below.
#[test]
fn the_disclosure_names_googles_agent_its_terms_and_its_measured_cost() {
    let model = disclosure_model();
    let frame = draw(&model, 160, 50);
    for needle in [
        "the sign-in is Google's, not Haider's",
        "antigravity-acp agent performs the OAuth and keeps the token",
        "proprietary Google software",
        "antigravity.google/terms",
        "~316 MB",
        "~682 MB",
        "~885 MiB",
        "~2.0 GB",
        "~225 MiB",
        "15 s",
        "[1] install Google's agent and sign in · [2] cancel",
    ] {
        assert!(frame.contains(needle), "the card states {needle:?}");
    }
}

// ---- 3. the acknowledgement --------------------------------------------

/// LAW — proceeding journals the acknowledgement EXACTLY ONCE and then starts
/// the flow. A second sync writes nothing: the journal is idempotent per
/// subject, so a re-affirmed decision never doubles its record.
///
/// MUTATION CHECK: drop the `subjects().contains(subject)` guard from
/// `TermsJournal::record`. Expected runtime failure: the second sync appends
/// a duplicate and the line count assertion fails.
#[test]
fn proceeding_journals_the_acknowledgement_exactly_once() {
    let directory = tempfile::tempdir().expect("temp dir");
    let journal = Some(TermsJournal::at(directory.path().join(TERMS_JOURNAL_FILE)));
    let mut model = disclosure_model();
    let mut seen = model.terms_ack_commits;

    model.handle(key(KeyCode::Char('1')));
    assert!(model.antigravity_consent.is_none(), "the card closed");
    assert!(
        model
            .acknowledged_terms
            .contains(GOOGLE_ANTIGRAVITY_TERMS_SUBJECT)
    );
    let card = model.oauth_add.as_ref().expect("the flow started");
    assert_eq!(card.provider, GOOGLE_ANTIGRAVITY_PROVIDER);
    assert_eq!(card.title, "Google Antigravity");

    sync_terms_persistence(&model, &mut seen, &journal);
    sync_terms_persistence(&model, &mut seen, &journal);
    let path = directory.path().join(TERMS_JOURNAL_FILE);
    let text = std::fs::read_to_string(&path).expect("the journal was written");
    assert_eq!(text.lines().count(), 1, "exactly one record: {text}");

    // A later boot reads it back and the disclosure never returns.
    let reloaded = TermsJournal::at(path);
    assert!(
        reloaded
            .subjects()
            .contains(GOOGLE_ANTIGRAVITY_TERMS_SUBJECT)
    );
    assert!(
        !reloaded.record(
            GOOGLE_ANTIGRAVITY_TERMS_SUBJECT,
            GOOGLE_ANTIGRAVITY_TERMS_WARNING,
            GENERATED_AT_MS
        ),
        "an already-recorded subject is never appended twice"
    );
}

/// Declining leaves NO record of an acceptance that never happened, and
/// starts no flow — so nothing is downloaded either.
#[test]
fn declining_journals_nothing_and_starts_no_flow() {
    let directory = tempfile::tempdir().expect("temp dir");
    let journal = Some(TermsJournal::at(directory.path().join(TERMS_JOURNAL_FILE)));
    let mut model = disclosure_model();
    let mut seen = model.terms_ack_commits;

    model.handle(key(KeyCode::Char('2')));
    assert!(model.antigravity_consent.is_none(), "the card closed");
    assert!(model.oauth_add.is_none(), "no flow started");
    assert!(model.acknowledged_terms.is_empty());
    assert_eq!(model.terms_ack_commits, 0);
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::OAuthAddStart { .. }))
    );

    sync_terms_persistence(&model, &mut seen, &journal);
    assert!(
        !directory.path().join(TERMS_JOURNAL_FILE).exists(),
        "a declined warning writes no file at all"
    );
}

/// Esc is the same decline as `[2]` — the card owns the key, so it never
/// falls through to `exit_accounts` and strands a half-answered warning.
#[test]
fn esc_declines_the_disclosure_without_leaving_the_screen() {
    let mut model = disclosure_model();
    model.handle(key(KeyCode::Esc));
    assert!(model.antigravity_consent.is_none());
    assert_eq!(model.screen, Screen::Accounts, "esc answered the card only");
    assert!(model.acknowledged_terms.is_empty());
}

/// SECURITY — the record carries the subject, the instant and the warning
/// text, and nothing else. No URL, no query, no code, no token, no identity.
///
/// MUTATION CHECK: add any credential-shaped field to `AcknowledgementDto`.
/// Expected runtime failure: the exact key-set assertion below.
#[test]
fn the_journal_entry_carries_no_url_query_or_token() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join(TERMS_JOURNAL_FILE);
    let journal = TermsJournal::at(path.clone());
    assert!(journal.record(
        GOOGLE_ANTIGRAVITY_TERMS_SUBJECT,
        GOOGLE_ANTIGRAVITY_TERMS_WARNING,
        GENERATED_AT_MS
    ));
    let text = std::fs::read_to_string(&path).expect("journal");
    for forbidden in [
        "http",
        "://",
        "?",
        "code=",
        "token",
        "Bearer",
        "access_",
        "refresh",
        "client_id",
        "oauth2",
        "accounts.google.com",
    ] {
        assert!(
            !text.contains(forbidden),
            "the record must not contain {forbidden:?}: {text}"
        );
    }
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("one JSON object");
    let object = value.as_object().expect("object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["acknowledged_at_ms", "subject", "version", "warning"],
        "the record's whole vocabulary"
    );
    assert_eq!(object["warning"], GOOGLE_ANTIGRAVITY_TERMS_WARNING);
    assert_eq!(object["subject"], GOOGLE_ANTIGRAVITY_TERMS_SUBJECT);
}

/// A profile that already acknowledged goes straight to the flow: the owner
/// asked for ONE warning before the first login, then the standing badge.
#[test]
fn an_acknowledged_profile_skips_the_disclosure() {
    let mut model = launcher_model();
    model
        .acknowledged_terms
        .insert(GOOGLE_ANTIGRAVITY_TERMS_SUBJECT.to_owned());
    run_slash(&mut model, "/login google");
    assert!(
        model.antigravity_consent.is_none(),
        "no second interstitial"
    );
    let card = model.oauth_add.as_ref().expect("the flow started");
    assert_eq!(card.provider, GOOGLE_ANTIGRAVITY_PROVIDER);
    assert_eq!(model.terms_ack_commits, 0, "nothing new to journal");
}

// ---- 4. the flow's copy ------------------------------------------------

/// SECURITY + honesty — the running card names Google's agent as the party
/// performing the sign-in, and renders NO sign-in URL: only the fact that a
/// browser was opened.
///
/// MUTATION CHECK: render `url` instead of `origin` in the `WaitingBrowser`
/// arm. Expected runtime failure: the loopback URL appears on screen.
#[test]
fn the_agent_owned_card_names_googles_agent_and_renders_no_url() {
    let mut model = launcher_model();
    model
        .acknowledged_terms
        .insert(GOOGLE_ANTIGRAVITY_TERMS_SUBJECT.to_owned());
    run_slash(&mut model, "/login google");
    let attempt = model.oauth_add.as_ref().expect("card open").attempt;
    model.oauth_add_phase(
        attempt,
        OAuthAddPhase::WaitingBrowser {
            url: "https://accounts.google.com/o/oauth2/v2/auth?client_id=secret&code=leak"
                .to_owned(),
            origin: "Google's own antigravity-acp agent".to_owned(),
        },
    );
    let frame = draw(&model, 160, 50);
    assert!(frame.contains("OAuth (Google's own agent)"));
    assert!(frame.contains("the agent keeps the token, Haider never sees it"));
    // Nothing anywhere on the frame carries the URL, its host, its query or
    // the code — the card holds the only lines that could, and the whole
    // screen is checked so a stray receipt or flash cannot leak them either.
    for forbidden in [
        "accounts.google.com",
        "client_id",
        "code=",
        "oauth2",
        "https://",
        "?",
        "&",
        "leak",
        "secret",
    ] {
        assert!(
            !frame.contains(forbidden),
            "no URL material reaches the screen: {forbidden:?}"
        );
    }
}

/// The `Starting` phase reports the install honestly rather than claiming a
/// loopback Haider never opens — and says the download is happening, so it
/// is never a silent background fetch.
#[test]
fn the_starting_phase_reports_the_install_instead_of_a_loopback() {
    let mut model = launcher_model();
    model
        .acknowledged_terms
        .insert(GOOGLE_ANTIGRAVITY_TERMS_SUBJECT.to_owned());
    run_slash(&mut model, "/login google");
    let frame = draw(&model, 160, 50);
    assert!(frame.contains("installing Google's agent if it is not present"));
    assert!(!frame.contains("starting the loopback flow"));
}

// ---- 5. the accounts screen -------------------------------------------

/// LAW — a Google account wears the `google-antigravity (ACP)` source badge
/// with a MASKED identity, and the screen carries the standing terms warning
/// verbatim for as long as that account exists.
///
/// MUTATION CHECK: return `kind.replace('_', " ")` for every kind in
/// `account_source_kind_label`. Expected runtime failure: the badge reads
/// `[google antigravity]` and the assertion below fails.
#[test]
fn the_accounts_screen_badges_google_and_carries_the_standing_warning() {
    let model = google_accounts_model();
    let frame = draw(&model, 160, 50);
    assert!(
        frame.contains(
            "[google-antigravity (ACP)] Google's antigravity-acp agent · google agent profile · refresh: antigravity acp · ready"
        ),
        "the badge names the protocol and who owns the refresh"
    );
    let masked = haider_tui::format::mask_identity("pilot@example.com");
    assert!(
        frame.contains(&masked),
        "the identity rides the one mask authority ({masked})"
    );
    assert!(
        !frame.contains("pilot@example.com"),
        "the raw identity never renders unrevealed"
    );
    // Haider never sees the token, so it never claims to know these.
    assert!(frame.contains("plan unknown · refreshed unknown · expires unknown · seen unknown"));
    // The standing badge carries the warning verbatim at every owner width,
    // exactly like the first-login card does.
    for (width, height) in SIZES {
        assert_eq!(
            rendered_warning(&draw(&model, width, height)),
            GOOGLE_ANTIGRAVITY_TERMS_WARNING,
            "the standing warning survives wrapping at {width}x{height}"
        );
    }
}

/// The standing warning is scoped to the accounts that need it: no Google
/// account, no badge line.
#[test]
fn the_standing_warning_is_absent_without_a_google_account() {
    let mut model = accounts_model();
    model.accounts.apply_snapshot(
        vec![AccountRow::from_descriptor(&descriptor(
            "anthropic-oauth",
            "anthropic-oauth",
            "you@work.com",
        ))],
        Some(3),
    );
    let frame = draw(&model, 160, 50);
    assert!(!frame.contains("terms warning"));
    assert!(!frame.contains("google-antigravity (ACP)"));
}

/// A daemon that DOES enrol a source for the account keeps it: the derived
/// badge fills a gap, it never duplicates daemon truth.
///
/// MUTATION CHECK: drop the "no joined source" filter from `derived_sources`.
/// Expected runtime failure: two badge lines for one account.
#[test]
fn a_daemon_supplied_google_source_is_not_badged_twice() {
    let mut model = google_accounts_model();
    model.accounts.apply_sources(vec![AccountSourceRow {
        source_id: "src1_google".into(),
        account_alias: Some(CredentialAlias::new("google-antigravity")),
        kind: GOOGLE_ANTIGRAVITY_SOURCE_KIND.into(),
        label: "Antigravity profile".into(),
        path: None,
        credential_store: "file".into(),
        refresh_owner: "antigravity_acp".into(),
        health: "ready".into(),
        last_seen_at_ms: None,
        last_refreshed_at_ms: None,
        access_expires_at_ms: None,
        plan: None,
        masked_identity: Some("p***@e***.com".into()),
    }]);
    let frame = draw(&model, 160, 50);
    let badges = frame
        .rows
        .iter()
        .filter(|row| row.contains("[google-antigravity (ACP)]"))
        .count();
    assert_eq!(badges, 1, "one badge, and it is the daemon's");
    assert!(frame.contains("[google-antigravity (ACP)] Antigravity profile"));
}

/// The `/accounts` add row offers the same door the command does, so the
/// screen carrying the warning can also add the account.
#[test]
fn the_accounts_add_row_offers_google_antigravity() {
    let model = accounts_model();
    let frame = draw(&model, 160, 50);
    assert!(frame.contains("[+ Google Antigravity (OAuth)]"));
    assert!(
        frame.has_hit(|hit| matches!(
            hit,
            haider_tui::app::Hit::AccountAdd(AccountAddKind::GoogleAntigravity)
        )),
        "the button carries its add hit"
    );
}

// ---- 6. the meter ------------------------------------------------------

/// LAW — ACP publishes no quota, so the meter renders UNAVAILABLE with its
/// machine reason. Never a zero, never a bar, never an invented percentage.
///
/// MUTATION CHECK: map the Google account onto `Metered { windows: vec![] }`.
/// Expected runtime failure: the row reads "metered · no windows published"
/// and the unavailable assertion fails.
#[test]
fn the_google_meter_renders_unavailable_with_its_reason_never_a_zero() {
    let model = usage_model(
        AccountMeterStateV1::Unavailable {
            reason: NO_QUOTA_REASON.into(),
        },
        UsageScope::Accounts,
    );
    let frame = draw(&model, 160, 50);
    let start = frame
        .row_containing("google-antigravity")
        .expect("the account block");
    let block: Vec<String> = frame.rows[start..]
        .iter()
        .map(|row| row.trim().to_owned())
        .take_while(|row| !row.is_empty())
        .collect();
    assert!(
        block
            .iter()
            .any(|row| row.contains("meter unavailable · acp agent publishes no quota")),
        "the typed reason survives to the row: {block:?}"
    );
    assert!(
        block.iter().all(|row| !row.contains('%')),
        "absent provider data is never a percentage: {block:?}"
    );
    assert!(
        block
            .iter()
            .all(|row| !row.contains('▰') && !row.contains('▱')),
        "no bar is drawn for a meter that does not exist: {block:?}"
    );
}

/// The calendar the `oauthcapture` wave added says `reset unknown` for a
/// Google account rather than inventing a window from Google's published
/// cadence — and marks no day for it.
#[test]
fn the_calendar_says_reset_unknown_for_a_google_account() {
    let model = usage_model(
        AccountMeterStateV1::Unavailable {
            reason: NO_QUOTA_REASON.into(),
        },
        UsageScope::Calendar,
    );
    let frame = draw(&model, 118, 36);
    assert!(frame.contains(
        "5h reset unknown · weekly reset unknown · meter unavailable · acp agent publishes no quota"
    ));
    // The grid itself — between the weekday header and the marker legend —
    // must carry no marker at all: an unavailable meter has no reset to plot.
    let grid_start = frame.row_containing("Sun").expect("weekday header");
    let grid_end = frame
        .row_containing("RESET MARKERS")
        .expect("marker legend");
    for row in &frame.rows[grid_start + 1..grid_end] {
        assert!(
            !row.contains("a5") && !row.contains("aW"),
            "an unmetered account marks no calendar day: {row:?}"
        );
    }
}

/// The calendar still plots a REAL published reset beside the Google row —
/// proof the pins above hold Google's honesty, not a dead calendar. And a
/// Google account that somehow arrived METERED with the very same window
/// still plots nothing: `calendar_reset_window` maps no window name for this
/// provider, so Google's published cadence can never become a timestamp.
///
/// MUTATION CHECK: add a `google` arm to `calendar_reset_window`'s name
/// table. Expected runtime failure: a `b5` marker appears on 04 September for
/// a provider that publishes no reset at all.
#[test]
fn the_calendar_plots_a_real_reset_but_never_infers_a_google_one() {
    let model = usage_model_from(
        vec![
            account_report(
                "openai-oauth",
                "codex-work",
                AccountMeterStateV1::Metered {
                    windows: vec![five_hour_window()],
                },
            ),
            account_report(
                GOOGLE_ANTIGRAVITY_PROVIDER,
                "google-antigravity",
                AccountMeterStateV1::Metered {
                    windows: vec![five_hour_window()],
                },
            ),
        ],
        UsageScope::Calendar,
    );
    let frame = draw(&model, 118, 36);
    assert!(frame.contains("[04] a5"), "the OpenAI reset is plotted");
    assert!(
        !frame.contains("b5") && !frame.contains("bW"),
        "the Google account plots no day"
    );
    assert!(frame.contains("5h reset unknown · exact window not published"));
}

// ---- 7. goldens --------------------------------------------------------

#[test]
fn antigravity_consent_dark_goldens_at_owner_widths() {
    let mut model = disclosure_model();
    model.theme = haider_tui::theme::ThemeKey::Dark;
    for (width, height) in SIZES {
        check_golden("antigravity_consent_dark", &draw(&model, width, height));
    }
}

#[test]
fn antigravity_consent_light_goldens_at_owner_widths() {
    let mut model = disclosure_model();
    model.theme = haider_tui::theme::ThemeKey::Light;
    for (width, height) in SIZES {
        check_golden("antigravity_consent_light", &draw(&model, width, height));
    }
}

#[test]
fn antigravity_accounts_dark_goldens_at_owner_widths() {
    let mut model = google_accounts_model();
    model.theme = haider_tui::theme::ThemeKey::Dark;
    for (width, height) in SIZES {
        check_golden("antigravity_accounts_dark", &draw(&model, width, height));
    }
}

#[test]
fn antigravity_accounts_light_goldens_at_owner_widths() {
    let mut model = google_accounts_model();
    model.theme = haider_tui::theme::ThemeKey::Light;
    for (width, height) in SIZES {
        check_golden("antigravity_accounts_light", &draw(&model, width, height));
    }
}

#[test]
fn antigravity_meter_dark_goldens_at_owner_widths() {
    let mut model = usage_model(
        AccountMeterStateV1::Unavailable {
            reason: NO_QUOTA_REASON.into(),
        },
        UsageScope::Calendar,
    );
    model.theme = haider_tui::theme::ThemeKey::Dark;
    for (width, height) in SIZES {
        check_golden("antigravity_meter_dark", &draw(&model, width, height));
    }
}

#[test]
fn antigravity_meter_light_goldens_at_owner_widths() {
    let mut model = usage_model(
        AccountMeterStateV1::Unavailable {
            reason: NO_QUOTA_REASON.into(),
        },
        UsageScope::Calendar,
    );
    model.theme = haider_tui::theme::ThemeKey::Light;
    for (width, height) in SIZES {
        check_golden("antigravity_meter_light", &draw(&model, width, height));
    }
}
