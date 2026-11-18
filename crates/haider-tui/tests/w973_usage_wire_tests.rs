//! Final usage-wire projection and the existing approval-card request seam.
#![allow(clippy::expect_used)]
use haider_protocol::EventPayload;
use haider_protocol::credential::AuthMethod;
use haider_protocol::ids::{CredentialAlias, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::usage::*;
use haider_rpc::CommandId;
use haider_tui::app::{AppEvent, AppModel, AppRequest, RuntimeMode, Screen, UsageScope};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::theme::{ThemeKey, readable_fg, reset_badge_fg};
use haider_tui::usage_apply::UsageApplyState;
use haider_tui::usage_detail::{BankedResetAction, account_extras, detail_rows};
use haider_tui::usage_time::UsageTimeZone;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const NOW: u64 = 1_790_182_800_000;
fn credits() -> UsageResetCreditsV1 {
    UsageResetCreditsV1 {
        available_count: 2,
        applicable_available_count: None,
        total_earned_count: None,
        used_count: None,
        remaining_count: None,
        credits: vec![],
        apply_supported: true,
    }
}
fn account() -> AccountUsageReportV1 {
    AccountUsageReportV1 {
        provider: "openai-oauth".into(),
        alias: CredentialAlias::new("synthetic-codex"),
        identity: None,
        plan: None,
        auth_method: AuthMethod::OAuth,
        meter: AccountMeterStateV1::Metered { windows: vec![] },
        reset_credits: None,
        previous_resets: vec![],
        local: LocalUsageStatsV1::default(),
    }
}
fn rows(account: &AccountUsageReportV1) -> Vec<haider_tui::usage_detail::DetailRow> {
    detail_rows(
        account,
        &account_extras(account),
        UsageTimeZone::Fixed(12600),
        NOW,
        2026,
    )
}
fn row(account: &AccountUsageReportV1, label: &str) -> String {
    rows(account)
        .into_iter()
        .find(|row| row.label == label)
        .expect("label")
        .text
}
#[test]
fn absent_inventory_and_optional_counts_never_become_zero() {
    let mut a = account();
    assert_eq!(row(&a, "banked resets"), "not reported by provider");
    assert_eq!(account_extras(&a).action, BankedResetAction::NotReported);
    a.reset_credits = Some(credits());
    assert_eq!(row(&a, "banked resets"), "available 2");
    for label in ["applicable", "total earned", "used", "remaining"] {
        assert_eq!(row(&a, label), "not reported", "{label}");
    }
    assert_eq!(account_extras(&a).action, BankedResetAction::Available);
}
#[test]
fn every_count_keeps_the_full_u64_range_including_zero() {
    for count in [0, 1, u64::from(u32::MAX) + 1, u64::MAX] {
        let mut c = credits();
        c.available_count = count;
        c.applicable_available_count = Some(count);
        c.total_earned_count = Some(count);
        c.used_count = Some(count);
        c.remaining_count = Some(count);
        let mut a = account();
        a.reset_credits = Some(c);
        assert_eq!(row(&a, "banked resets"), format!("available {count}"));
        for label in ["applicable", "total earned", "used", "remaining"] {
            assert_eq!(row(&a, label), count.to_string(), "{label}");
        }
    }
}
#[test]
fn action_requires_provider_support_and_nonzero_known_available_counts() {
    let mut a = account();
    let mut c = credits();
    c.apply_supported = false;
    a.reset_credits = Some(c.clone());
    assert_eq!(account_extras(&a).action, BankedResetAction::NotOffered);
    c.apply_supported = true;
    c.available_count = 0;
    a.reset_credits = Some(c.clone());
    assert!(matches!(
        account_extras(&a).action,
        BankedResetAction::Unavailable { .. }
    ));
    c.available_count = 2;
    c.applicable_available_count = Some(0);
    a.reset_credits = Some(c);
    assert!(matches!(
        account_extras(&a).action,
        BankedResetAction::Unavailable { .. }
    ));
}
#[test]
fn every_credit_field_and_exact_instant_is_rendered() {
    let mut a = account();
    let mut c = credits();
    c.credits.push(UsageResetCreditV1 {
        reset_type: "banked".into(),
        status: "available".into(),
        granted_at_ms: Some(NOW - 3600000),
        expires_at_ms: Some(NOW + 3600000),
        redeemed_at_ms: None,
        title: Some("earned credit".into()),
        description: Some("provider description".into()),
    });
    a.reset_credits = Some(c);
    assert_eq!(
        row(&a, "credit details"),
        "1 · type banked · status available"
    );
    assert_eq!(row(&a, "title"), "earned credit");
    assert_eq!(row(&a, "description"), "provider description");
    assert!(row(&a, "granted").contains("local (16:00 UTC) · 1h 0m ago"));
    assert!(row(&a, "expires").contains("local (18:00 UTC) · in 1h 0m"));
    assert_eq!(row(&a, "redeemed"), "not reported by provider");
    a.reset_credits.as_mut().expect("credits").credits[0].redeemed_at_ms = Some(NOW);
    assert!(row(&a, "redeemed").contains("17:00 UTC"));
}
#[test]
fn history_preserves_optional_window_kind_observation_and_all_provenances() {
    for (source, label) in [
        (UsageResetHistorySourceV1::Provider, "provider"),
        (
            UsageResetHistorySourceV1::DeviceJournal,
            "this device's meter journal",
        ),
        (UsageResetHistorySourceV1::Unknown, "source unknown"),
    ] {
        let mut a = account();
        a.previous_resets = vec![UsageResetHistoryEntryV1 {
            reset_at_ms: NOW - 3600000,
            source,
            window: None,
            observed_at_ms: Some(NOW),
            kind: Some("banked_reset".into()),
        }];
        assert!(row(&a, "previous resets").starts_with("window not reported"));
        assert!(row(&a, "previous resets").ends_with(label));
        assert_eq!(row(&a, "reset kind"), "banked_reset");
        assert!(row(&a, "observed").contains("local (17:00 UTC)"));
        a.previous_resets[0].window = Some("weekly".into());
        assert!(row(&a, "previous resets").starts_with("weekly "));
    }
}
#[test]
fn history_empty_and_omitted_share_the_same_wire_truth() {
    let a = account();
    let mut value = serde_json::to_value(&a).expect("serialize");
    value
        .as_object_mut()
        .expect("object")
        .remove("previous_resets");
    let omitted: AccountUsageReportV1 = serde_json::from_value(value.clone()).expect("omitted");
    value["previous_resets"] = serde_json::json!([]);
    let empty: AccountUsageReportV1 = serde_json::from_value(value).expect("empty");
    assert_eq!(
        row(&omitted, "previous resets"),
        "no entries reported (empty or omitted)"
    );
    assert_eq!(
        row(&empty, "previous resets"),
        "no entries reported (empty or omitted)"
    );
    assert_eq!(account_extras(&omitted), account_extras(&empty));
}
#[test]
fn all_history_and_credit_records_are_reachable_without_a_silent_cap() {
    let mut a = account();
    for index in 0..12 {
        a.previous_resets.push(UsageResetHistoryEntryV1 {
            reset_at_ms: NOW - index * 3600000,
            source: UsageResetHistorySourceV1::Provider,
            window: Some(format!("window-{index}")),
            kind: None,
            observed_at_ms: None,
        });
    }
    let text = rows(&a)
        .into_iter()
        .map(|r| r.text)
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..12 {
        assert!(text.contains(&format!("window-{index} ")));
    }
}
#[test]
fn retry_instant_is_absent_past_future_or_per_instant_dst_with_utc_and_relative() {
    let mut a = account();
    for at in [None, Some(NOW - 3600000), Some(NOW + 3600000)] {
        a.meter = AccountMeterStateV1::Unavailable {
            reason: "http_status_429".into(),
            retry_at_ms: at,
        };
        let text = row(&a, "retry at");
        if at.is_some() {
            assert!(text.contains("local (") && text.contains("UTC)"));
        } else {
            assert_eq!(text, "not reported by provider");
        }
    }
    let transition = 1_793_512_800_000;
    fn offset(at: i64) -> Option<i32> {
        Some(if at < 1_793_512_800 { -14400 } else { -18000 })
    }
    for at in [transition - 60000, transition + 60000] {
        a.meter = AccountMeterStateV1::Unavailable {
            reason: "http_status_429".into(),
            retry_at_ms: Some(at),
        };
        let text = detail_rows(
            &a,
            &account_extras(&a),
            UsageTimeZone::Rule(offset),
            transition,
            2026,
        )
        .into_iter()
        .find(|r| r.label == "retry at")
        .expect("retry")
        .text;
        assert!(text.contains("UTC)"));
        assert!(
            text.contains(if at < transition {
                "01:59 local"
            } else {
                "01:01 local"
            }),
            "{text}"
        );
    }
}
#[test]
fn selected_reset_badge_text_meets_aa_in_every_theme() {
    for key in ThemeKey::ALL {
        let theme = key.theme();
        assert!(
            reset_badge_fg(theme).contrast(theme.ok) >= 4.5,
            "{}",
            key.name()
        );
        assert!(theme.dim.contrast(theme.bg) >= 4.5, "{}", key.name());
        assert!(
            readable_fg(theme.err, theme.bg).contrast(theme.bg) >= 4.5,
            "{} calendar error",
            key.name()
        );
        for ink in [theme.text, theme.bright, theme.dim, theme.warn] {
            assert!(ink.contrast(theme.bg) >= 4.5, "{} body ink", key.name());
        }
        for background in [theme.bg, theme.bar_bg, theme.sel_bg, theme.gold_soft] {
            assert!(
                readable_fg(theme.gold, background).contrast(background) >= 4.5,
                "{} calendar accent",
                key.name()
            );
        }
        assert!(
            readable_fg(theme.dim, theme.gold_soft).contrast(theme.gold_soft) >= 4.5,
            "{} other reset marker",
            key.name()
        );
    }
}
fn live_calendar() -> AppModel {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Usage;
    model.active_session = Some(SessionId::new("synthetic-session"));
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_USAGE_RESET_APPLY_V1.into());
    let mut a = account();
    a.reset_credits = Some(credits());
    model.usage.apply_report(UsageReportV1 {
        generated_at_ms: NOW,
        accounts: vec![a],
        orchestration: Default::default(),
    });
    model.usage.scope = UsageScope::Calendar;
    model
}
#[test]
fn b_only_requests_the_existing_daemon_approval_and_never_answers_a_card() {
    let mut model = live_calendar();
    model.composer.set_text("unsent synthetic draft");
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('b'),
        KeyModifiers::NONE,
    )));
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.composer.text(), "unsent synthetic draft");
    assert!(model.outbox.is_empty());
    assert!(
        model.projection.open_menu().is_none(),
        "the UI must not mint a second card"
    );
    let mut driver = LiveDriver::new("usage-ui-wire");
    let requests = std::mem::take(&mut model.requests);
    let commands = requests
        .into_iter()
        .flat_map(|r| driver.handle_request(&mut model, r))
        .collect::<Vec<_>>();
    assert_eq!(commands.len(), 1);
    assert!(
        matches!(&commands[0], LiveCommand::UsageResetApply { account, .. } if account.as_str() == "synthetic-codex")
    );
    assert!(matches!(
        request_body(commands[0].clone()),
        haider_rpc::RequestBody::UsageResetApply { .. }
    ));
    let context = CommandContext::of(&commands[0]);
    let result = receipt(UsageResetApplyStatusV1::ApprovalPending);
    let replies = map_response(
        &context,
        haider_rpc::ResponseBody::UsageResetApply {
            result,
            menu_id: haider_protocol::ids::MenuId::new("synthetic-menu"),
            request_seq: 2,
            worker_generation: 1,
        },
    );
    for reply in replies {
        driver.apply(&mut model, reply);
    }
    assert!(
        model
            .usage
            .reset_apply
            .pending(&CredentialAlias::new("synthetic-codex"))
    );
    assert!(
        driver
            .handle_request(
                &mut model,
                AppRequest::UsageResetApply {
                    session: SessionId::new("synthetic-session"),
                    account: CredentialAlias::new("synthetic-codex")
                }
            )
            .is_empty()
    );
    assert!(model.outbox.is_empty());
}
fn receipt(status: UsageResetApplyStatusV1) -> UsageResetApplyResultV1 {
    UsageResetApplyResultV1 {
        approval_id: "synthetic-approval".into(),
        account: CredentialAlias::new("synthetic-codex"),
        status,
        windows_reset: None,
        reason: None,
    }
}
#[test]
fn daemon_capability_failure_releases_the_request_latch_without_an_answer() {
    let mut model = live_calendar();
    model.daemon_features.clear();
    let mut driver = LiveDriver::new("usage-ui-unavailable");
    assert!(
        driver
            .handle_request(
                &mut model,
                AppRequest::UsageResetApply {
                    session: SessionId::new("synthetic-session"),
                    account: CredentialAlias::new("synthetic-codex")
                }
            )
            .is_empty()
    );
    assert!(
        !model
            .usage
            .reset_apply
            .pending(&CredentialAlias::new("synthetic-codex"))
    );
    assert!(model.outbox.is_empty());
}

#[test]
fn reset_request_checks_capability_again_at_the_socket_send_boundary() {
    let mut model = live_calendar();
    let mut driver = LiveDriver::new("reset-reconnect-capability");
    let alias = CredentialAlias::new("synthetic-codex");
    let commands = driver.handle_request(
        &mut model,
        AppRequest::UsageResetApply {
            session: SessionId::new("synthetic-session"),
            account: alias.clone(),
        },
    );
    assert_eq!(
        haider_tui::link::command_required_features(&commands[0]),
        &[haider_rpc::FEATURE_USAGE_RESET_APPLY_V1],
        "a queued request must recheck the newly connected daemon before sending"
    );
    driver.apply(
        &mut model,
        LiveReply::Failed {
            command_id: commands[0].command_id().cloned(),
            code: "feature_missing".into(),
            message: "connected daemon does not advertise reset approval".into(),
            retryable: false,
            presentation: None,
        },
    );
    assert!(!model.usage.reset_apply.pending(&alias));
    assert!(
        model
            .usage
            .reset_apply
            .rows(&alias)
            .iter()
            .any(|row| row.text.contains("feature_missing"))
    );
    assert!(model.outbox.is_empty());
}
#[test]
fn terminal_receipts_survive_late_pending_replies_and_preserve_typed_reasons() {
    let session = SessionId::new("synthetic-session");
    for (status, label) in [
        (UsageResetApplyStatusV1::Applied, "applied"),
        (UsageResetApplyStatusV1::Denied, "denied"),
        (UsageResetApplyStatusV1::Failed, "failed"),
    ] {
        let mut state = UsageApplyState::default();
        let mut terminal = receipt(status);
        terminal.reason = Some("account_changed".into());
        terminal.windows_reset = Some(0);
        let payload = EventPayload::Item(ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("synthetic-result"),
            item: TurnItem::Extension {
                kind: USAGE_RESET_APPLY_RESULT_EXTENSION.into(),
                data: serde_json::to_value(&terminal).expect("receipt"),
            },
        });
        state.observe(&session, &payload);
        state.record(&session, receipt(UsageResetApplyStatusV1::ApprovalPending));
        assert!(!state.pending(&terminal.account));
        let rows = state.rows(&terminal.account);
        assert!(rows.iter().any(|r| r.text.starts_with(label)));
        assert!(rows.iter().any(|r| r.text == "account_changed"));
        assert!(
            rows.iter()
                .any(|r| r.label == "windows reset" && r.text == "0")
        );
        assert!(
            state
                .rows(&CredentialAlias::new("another-account"))
                .is_empty()
        );
    }
}
#[test]
fn initial_receipt_maps_to_its_issuing_session_and_command() {
    let command = LiveCommand::UsageResetApply {
        command_id: CommandId::new("synthetic-command"),
        session: SessionId::new("synthetic-session"),
        account: CredentialAlias::new("synthetic-codex"),
    };
    assert_eq!(
        command.command_id(),
        Some(&CommandId::new("synthetic-command"))
    );
    let response = haider_rpc::ResponseBody::UsageResetApply {
        result: receipt(UsageResetApplyStatusV1::ApprovalPending),
        menu_id: haider_protocol::ids::MenuId::new("synthetic-menu"),
        request_seq: 2,
        worker_generation: 1,
    };
    let replies = map_response(&CommandContext::of(&command), response.clone());
    assert!(
        matches!(replies.as_slice(), [LiveReply::UsageResetApplyResult { command_id, session, .. }]
        if command_id.as_str() == "synthetic-command" && session.as_str() == "synthetic-session")
    );
    assert!(
        map_response(&CommandContext::of(&LiveCommand::UsageReport), response).is_empty(),
        "a reset response without its issuing context cannot invent a session"
    );
}

#[test]
fn failed_rpc_releases_only_its_account_latch_and_keeps_the_typed_reason() {
    let mut model = live_calendar();
    let mut driver = LiveDriver::new("failed-ui");
    let session = SessionId::new("synthetic-session");
    let first = CredentialAlias::new("synthetic-codex");
    let second = CredentialAlias::new("another-account");
    let commands = driver.handle_request(
        &mut model,
        AppRequest::UsageResetApply {
            session: session.clone(),
            account: first.clone(),
        },
    );
    driver.handle_request(
        &mut model,
        AppRequest::UsageResetApply {
            session: session.clone(),
            account: second.clone(),
        },
    );
    driver.apply(
        &mut model,
        LiveReply::Failed {
            command_id: commands[0].command_id().cloned(),
            code: "invalid_request".into(),
            message: "reset account identity is incomplete".into(),
            retryable: false,
            presentation: None,
        },
    );
    assert!(!model.usage.reset_apply.pending(&first));
    assert!(model.usage.reset_apply.pending(&second));
    let rows = model.usage.reset_apply.rows(&first);
    assert!(
        rows.iter()
            .any(|row| row.text.contains("invalid_request") && row.text.contains("/accounts"))
    );
    assert_eq!(
        driver
            .handle_request(
                &mut model,
                AppRequest::UsageResetApply {
                    session,
                    account: first
                }
            )
            .len(),
        1
    );
    assert!(model.outbox.is_empty());
}

#[test]
fn restored_pending_menu_uses_its_original_session_and_terminal_replay_settles_it() {
    use haider_protocol::menu::{Menu, MenuKind, MenuScope};
    let mut state = UsageApplyState::default();
    let alias = CredentialAlias::new("synthetic-codex");
    let session = SessionId::new("original-session");
    let menu = Menu {
        id: haider_protocol::ids::MenuId::new("restored-menu"),
        kind: MenuKind::DeferredApproval {
            approval_id: "synthetic-approval".into(),
            effect: haider_protocol::effect::EffectClass::UsageResetApply,
            account: Some(alias.clone()),
            binding: None,
        },
        title: "synthetic pending reset".into(),
        body: vec![],
        options: vec![],
        blocking: false,
        scope: MenuScope::Session,
        origin: "deferred_approval_v1".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    state.observe(&session, &EventPayload::MenuOpened(menu));
    assert_eq!(state.pending_session(&alias), Some(&session));
    state.record(&session, receipt(UsageResetApplyStatusV1::Denied));
    assert!(state.pending_session(&alias).is_none());
}

#[test]
fn provider_prose_keeps_empty_distinct_from_absent_and_sanitizes_terminal_controls() {
    let mut a = account();
    let mut c = credits();
    c.credits.push(UsageResetCreditV1 {
        reset_type: "banked".into(),
        status: "available".into(),
        granted_at_ms: None,
        expires_at_ms: None,
        redeemed_at_ms: None,
        title: Some(String::new()),
        description: Some("wide 字 e\u{301} 👩‍💻\n\u{1b}[31m detail".into()),
    });
    a.reset_credits = Some(c);
    assert_eq!(row(&a, "title"), "");
    let text = row(&a, "description");
    assert!(!text.chars().any(char::is_control));
    assert!(text.contains("字 e\u{301} 👩‍💻"));
    a.reset_credits.as_mut().expect("credits").credits[0].title = None;
    assert_eq!(row(&a, "title"), "not reported by provider");

    let mut state = UsageApplyState::default();
    let mut result = receipt(UsageResetApplyStatusV1::Failed);
    result.reason = Some("provider\n\u{1b}[31m reason".into());
    state.record(&SessionId::new("session\u{1b}[?25l"), result.clone());
    state.failed(result.account.clone(), "rpc\n\u{1b}[31m failure".into());
    assert!(
        state
            .rows(&result.account)
            .iter()
            .all(|row| !row.text.chars().any(char::is_control)),
        "receipt identifiers and transport/provider reasons must also be terminal-safe"
    );
}

#[test]
fn credit_counts_and_tail_details_remain_reachable_at_both_layouts_in_every_theme() {
    use haider_tui::render::render;
    use ratatui::{Terminal, backend::TestBackend};
    for theme in ThemeKey::ALL {
        for (width, height) in [(80, 24), (118, 36)] {
            let mut model = live_calendar();
            model.theme = theme;
            let a = &mut model.usage.report.as_mut().expect("report").accounts[0];
            a.reset_credits.as_mut().expect("credits").available_count = u64::MAX;
            a.reset_credits
                .as_mut()
                .expect("credits")
                .credits
                .push(UsageResetCreditV1 {
                    reset_type: "banked".into(),
                    status: "available".into(),
                    title: Some("tail-credit".into()),
                    description: Some("Unicode 字 e\u{301} 👩‍💻 ".repeat(25) + "PROVIDER-TAIL"),
                    granted_at_ms: None,
                    expires_at_ms: None,
                    redeemed_at_ms: None,
                });
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            let mut seen = String::new();
            for offset in 0..200usize {
                model.usage.follow_cursor.set(false);
                model.usage.scroll.set(offset);
                terminal
                    .draw(|f| {
                        render(&model, f);
                    })
                    .expect("draw");
                let buffer = terminal.backend().buffer();
                for y in 0..height {
                    for x in 0..width {
                        seen.push_str(buffer[(x, y)].symbol());
                    }
                    seen.push('\n');
                }
                if offset >= model.usage.scroll_max.get() {
                    break;
                }
            }
            assert!(
                seen.contains("18446744073709551615"),
                "{} {width}x{height}",
                theme.name()
            );
            assert!(seen.contains("tail-credit"));
            assert!(seen.contains("PROVIDER-TAIL"), "every suffix is reachable");
        }
    }
}

#[test]
fn history_tail_remains_reachable_beyond_terminal_scroll_coordinate_limits() {
    use haider_tui::render::render;
    use ratatui::{Terminal, backend::TestBackend};
    for (width, height) in [(80, 24), (118, 36)] {
        let mut model = live_calendar();
        let a = &mut model.usage.report.as_mut().expect("report").accounts[0];
        a.previous_resets = (0..22_000u64)
            .map(|index| UsageResetHistoryEntryV1 {
                reset_at_ms: NOW - index * 60_000,
                source: UsageResetHistorySourceV1::Provider,
                window: None,
                observed_at_ms: None,
                kind: Some(
                    if index == 21_999 {
                        "OLDEST-HISTORY-TAIL"
                    } else {
                        "banked"
                    }
                    .into(),
                ),
            })
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                render(&model, frame);
            })
            .expect("draw");
        assert!(model.usage.scroll_max.get() > usize::from(u16::MAX));
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::End,
            KeyModifiers::NONE,
        )));
        terminal
            .draw(|frame| {
                render(&model, frame);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..height {
            for x in 0..width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(
            text.contains("OLDEST-HISTORY-TAIL"),
            "{width}x{height} tail is reachable with End"
        );
    }
}

#[test]
fn concurrent_account_requests_keep_separate_latches_until_their_own_receipts() {
    let mut model = live_calendar();
    let mut driver = LiveDriver::new("concurrent-ui");
    let first = CredentialAlias::new("synthetic-codex");
    let second = CredentialAlias::new("another-account");
    let session = SessionId::new("synthetic-session");
    for alias in [&first, &second] {
        assert_eq!(
            driver
                .handle_request(
                    &mut model,
                    AppRequest::UsageResetApply {
                        session: session.clone(),
                        account: alias.clone()
                    }
                )
                .len(),
            1
        );
    }
    assert!(model.usage.reset_apply.pending(&first));
    assert!(model.usage.reset_apply.pending(&second));
    assert!(
        driver
            .handle_request(
                &mut model,
                AppRequest::UsageResetApply {
                    session: session.clone(),
                    account: first.clone()
                }
            )
            .is_empty()
    );
    model
        .usage
        .reset_apply
        .record(&session, receipt(UsageResetApplyStatusV1::Denied));
    assert!(!model.usage.reset_apply.pending(&first));
    assert!(model.usage.reset_apply.pending(&second));
}

#[test]
fn unknown_result_is_visible_and_never_assumed_settled_for_a_second_request() {
    let mut state = UsageApplyState::default();
    let result = receipt(UsageResetApplyStatusV1::Unknown);
    state.record(&SessionId::new("synthetic-session"), result.clone());
    assert!(
        state.pending(&result.account),
        "an unknown future status is unresolved"
    );
    assert!(
        state
            .rows(&result.account)
            .iter()
            .any(|row| row.text.starts_with("unknown ·"))
    );
    assert!(
        state
            .rows(&result.account)
            .iter()
            .any(|row| row.label == "windows reset" && row.text == "not reported by provider")
    );
}

#[test]
fn old_receipt_replay_cannot_release_a_new_request_latch() {
    let session = SessionId::new("synthetic-session");
    let alias = CredentialAlias::new("synthetic-codex");
    let mut state = UsageApplyState::default();
    let old = receipt(UsageResetApplyStatusV1::Denied);
    state.record(&session, old.clone());
    state.begin(session.clone(), alias.clone());
    let payload = EventPayload::Item(ItemEvent::Completed {
        item_id: haider_protocol::ids::ItemId::new("old-result"),
        item: TurnItem::Extension {
            kind: USAGE_RESET_APPLY_RESULT_EXTENSION.into(),
            data: serde_json::to_value(old).expect("old receipt"),
        },
    });
    state.observe(&session, &payload);
    assert_eq!(state.pending_session(&alias), Some(&session));
    let mut current = receipt(UsageResetApplyStatusV1::ApprovalPending);
    current.approval_id = "new-approval".into();
    state.record(&session, current.clone());
    assert!(state.pending(&alias));
    current.status = UsageResetApplyStatusV1::Denied;
    state.record(&session, current);
    assert!(!state.pending(&alias));
}

#[test]
fn late_rpc_reply_cannot_release_another_command_latch() {
    let mut model = live_calendar();
    let mut driver = LiveDriver::new("late-reset-reply");
    let session = SessionId::new("synthetic-session");
    let alias = CredentialAlias::new("synthetic-codex");
    let request = AppRequest::UsageResetApply {
        session: session.clone(),
        account: alias.clone(),
    };
    let first = driver.handle_request(&mut model, request.clone());
    let old_reply = LiveReply::UsageResetApplyResult {
        command_id: first[0].command_id().expect("issued command").clone(),
        session: session.clone(),
        result: receipt(UsageResetApplyStatusV1::ApprovalPending),
    };
    driver.apply(&mut model, old_reply.clone());
    model
        .usage
        .reset_apply
        .record(&session, receipt(UsageResetApplyStatusV1::Denied));
    assert_eq!(driver.handle_request(&mut model, request).len(), 1);
    driver.apply(&mut model, old_reply);
    assert!(model.usage.reset_apply.pending(&alias));
    assert!(
        model
            .usage
            .reset_apply
            .rows(&alias)
            .iter()
            .any(|row| row.text.starts_with("requesting approval"))
    );
}

#[test]
fn page_keys_reach_every_credit_in_viewports_smaller_than_the_old_jump() {
    use haider_tui::render::render;
    use ratatui::{Terminal, backend::TestBackend};
    for (width, height) in [(80, 10), (118, 12)] {
        let mut model = live_calendar();
        let inventory = model.usage.report.as_mut().expect("report").accounts[0]
            .reset_credits
            .as_mut()
            .expect("credits");
        inventory.credits = (0..8)
            .map(|index| UsageResetCreditV1 {
                reset_type: "banked".into(),
                status: "available".into(),
                granted_at_ms: None,
                expires_at_ms: None,
                redeemed_at_ms: None,
                title: None,
                description: Some(format!("SMALL-TAIL-{index}")),
            })
            .collect();
        model.usage.follow_cursor.set(false);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        let mut pages = String::new();
        for _ in 0..200 {
            terminal
                .draw(|frame| {
                    render(&model, frame);
                })
                .expect("draw");
            let buffer = terminal.backend().buffer();
            for y in 0..height {
                for x in 0..width {
                    pages.push_str(buffer[(x, y)].symbol());
                }
                pages.push('\n');
            }
            let before = model.usage.scroll.get();
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::PageDown,
                KeyModifiers::NONE,
            )));
            if model.usage.scroll.get() == before {
                break;
            }
        }
        for index in 0..8 {
            assert!(
                pages.contains(&format!("SMALL-TAIL-{index}")),
                "{width}x{height} credit {index} was skipped"
            );
        }
    }
}

#[test]
fn receipt_display_and_pending_session_order_are_stable_across_arrival_order() {
    for order in [["session-z", "session-a"], ["session-a", "session-z"]] {
        let mut state = UsageApplyState::default();
        let account = CredentialAlias::new("synthetic-codex");
        for name in order {
            let session = SessionId::new(name);
            state.begin(session.clone(), account.clone());
        }
        assert_eq!(
            state.pending_session(&account).expect("pending").as_str(),
            "session-a"
        );
        for name in order {
            let mut result = receipt(UsageResetApplyStatusV1::ApprovalPending);
            result.approval_id = name.into();
            state.record(&SessionId::new(name), result);
        }
        assert_eq!(
            state.pending_session(&account).expect("receipt").as_str(),
            "session-a"
        );
        let sessions: Vec<_> = state
            .rows(&account)
            .into_iter()
            .filter(|row| row.label == "session")
            .map(|row| row.text)
            .collect();
        assert_eq!(sessions, ["session-a", "session-z"]);
    }
}
