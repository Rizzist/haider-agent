#![allow(clippy::expect_used)]

use haider_protocol::agent::{AgentMetricsSnapshot, AgentUsageMetrics};
use haider_protocol::ids::{RunId, SessionId};
use haider_protocol::provider::{
    CacheStatAvailability, NormalizedUsage, Usage, UsageRequestKind, UsageScope, UsageSource,
};
use haider_tui::app::{AppModel, Screen};
use haider_tui::cache_usage::{
    CacheUsageStatsExt as _, SessionUsageFold, medium_status, wide_status,
};
use haider_tui::plain::render_plain_with_cache;
use haider_tui::projection::SessionProjection;
use ratatui::{Terminal, backend::TestBackend};

fn usage(run: &str, provider: &str, kind: UsageRequestKind, normalized: NormalizedUsage) -> Usage {
    Usage {
        input: normalized.logical_input,
        output: normalized.billed_output,
        reasoning: normalized.reasoning_detail,
        cached: normalized.cache_read_input,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: Some(normalized),
        scope: Some(UsageScope {
            provider: provider.into(),
            model: "fixture-model".into(),
            account_scope: None,
            auth_scope: "api_key".into(),
            // 954 ledger dimensions: absent here as on any pre-ledger
            // fact — the fixture pins the shape, not the new lanes.
            api_family: None,
            effort: None,
            speed: None,
            cache_epoch: "epoch-fixture".into(),
            stable_prefix_tokens: 0,
            cache_boundaries: None,
            request_kind: kind,
            run: Some(RunId::new(run)),
            agent: None,
            prefix_digests: None,
        }),
        cache_cost: None,
        request: None,
    }
}

fn present(uncached: u64, read: u64, output: u64) -> NormalizedUsage {
    NormalizedUsage {
        logical_input: uncached.saturating_add(read),
        uncached_input: uncached,
        cache_read_input: read,
        billed_output: output,
        cache_status: CacheStatAvailability::Present,
        cache_telemetry_input: uncached.saturating_add(read),
        ..NormalizedUsage::default()
    }
}

fn draw(model: &AppModel, width: u16) -> String {
    let backend = TestBackend::new(width, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            haider_tui::render::render(model, frame);
        })
        .expect("render");
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

fn footer_model(reread_basis_points: Option<u32>) -> AppModel {
    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.cache_usage.note(&usage(
        "footer-authority",
        "openai",
        UsageRequestKind::MainTurn,
        // The locally derivable lifetime share is 7680 / (7680 + 4375) =
        // 63.71%, intentionally far from the published re-read rate.
        present(4_375, 7_680, 0),
    ));
    let session = SessionId::new("footer-authority");
    model.active_session = Some(session.clone());
    let metrics = AgentMetricsSnapshot {
        agent: None,
        session_id: session.clone(),
        head_seq: 1,
        started_at_ms: 0,
        terminal_at_ms: None,
        live: true,
        tool_attempts: 0,
        usage: Some(AgentUsageMetrics {
            cache_hit_basis_points: Some(6_370),
            cache_reread_hit_basis_points: reread_basis_points,
            ..AgentUsageMetrics::default()
        }),
    };
    model.note_summary_counts(&haider_rpc::SessionSummary {
        session_id: session,
        head_seq: 1,
        worker_generation: 1,
        run_state: None,
        run_id: None,
        seen_at_ms: None,
        last_activity_ms: None,
        waiting_why: None,
        needs_input: None,
        metadata: None,
        provider: None,
        last_model: None,
        cache_lifetime_hit_basis_points: Some(6_370),
        cache_reread_hit_basis_points: reread_basis_points,
        workspace_cwd: None,
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
        title: None,
        agent_metrics: Some(metrics),
        parent_session_id: None,
        kind: None,
        agent_type: None,
        effort: None,
        fast: None,
        account_alias: None,
    });
    // The nested copy was valid and equal when the summary arrived, but the
    // footer must remain driven by the independently hydrated promoted field.
    model.session_metrics.clear();
    model
}

/// MUTATION `FOOTER_LOCAL_LIFETIME_SHARE` (executed, observed red): feed the locally computed 63.71%
/// lifetime share to the formatter instead of the session's published re-read
/// field. Expected runtime failure: the authoritative 90.58% assertion.
#[test]
fn footer_uses_published_reread_rate_not_local_lifetime_share() {
    let line = haider_tui::render::status_left_string(&footer_model(Some(9_058)), 180);
    assert!(
        line.contains("⚡7.7k re-read 90.58%"),
        "footer must use the daemon's re-read authority: {line}"
    );
    assert!(
        !line.contains("63.71%") && !line.contains("63.70%"),
        "neither independently computed nor published lifetime share is cache health: {line}"
    );
}

/// MUTATION `ABSENT_REREAD_DEFAULTS_TO_ZERO` (executed, observed red): replace the `None` display with
/// `unwrap_or_default()`/0. Expected runtime failure: the n/a assertion.
#[test]
fn footer_absent_reread_rate_is_na_never_zero() {
    let line = haider_tui::render::status_left_string(&footer_model(None), 180);
    assert!(
        line.contains("⚡7.7k re-read n/a"),
        "no re-readable denominator must stay absent: {line}"
    );
    assert!(
        !line.contains("re-read 0.00%"),
        "absence must never look like total cache failure: {line}"
    );
}

/// MUTATION `REREAD_INTEGER_PERCENT` (executed, observed red): divide basis points as an integer before
/// formatting. Expected runtime failure: 9058 must retain both decimal places.
#[test]
fn footer_reread_basis_points_keep_two_decimal_places() {
    let totals = footer_model(Some(9_058)).cache_usage.totals();
    assert_eq!(
        wide_status(&totals, Some(9_058)),
        "↑4.4k ↓0 ⚡7.7k re-read 90.58%"
    );
    assert_eq!(medium_status(&totals, Some(9_058)), "⚡7.7k re-read 90.58%");
}

/// CM1g — token-weighted sample formatting is exact, latest cumulative
/// snapshots replace earlier updates, compaction remains a separate lane,
/// and partial coverage can never produce a complete session hit rate.
///
/// MUTATION CHECK (executed): sum streaming snapshots, average request
/// percentages, treat unavailable as zero, or remove request-kind from the
/// key; at least one exact total/rate assertion fails.
#[test]
fn cm1g_session_fold_and_responsive_cache_readout_laws() {
    let mut fold = SessionUsageFold::default();
    fold.note(&usage(
        "sample",
        "openai",
        UsageRequestKind::MainTurn,
        present(100, 100, 10),
    ));
    let mut latest = usage(
        "sample",
        "openai",
        UsageRequestKind::MainTurn,
        present(450_000, 108_800_000, 227_000),
    );
    let scope = latest.scope.as_mut().expect("fixture scope");
    scope.account_scope = Some(haider_protocol::ids::CredentialAlias::new("rotated"));
    scope.auth_scope = "api_key".into();
    fold.note(&latest);
    let totals = fold.totals();
    assert_eq!(totals.uncached_input_tokens, 450_000);
    assert_eq!(totals.cache_read_tokens, 108_800_000);
    assert_eq!(
        wide_status(&totals, Some(9_959)),
        "↑450k ↓227k ⚡108.8M re-read 99.59%"
    );
    assert_eq!(
        medium_status(&totals, Some(9_959)),
        "⚡108.8M re-read 99.59%"
    );

    fold.note(&usage(
        "sample",
        "openai",
        UsageRequestKind::Compaction,
        present(50_000, 0, 1_000),
    ));
    let with_compaction = fold.totals();
    assert_eq!(with_compaction.uncached_input_tokens, 500_000);
    assert!(with_compaction.breakdowns.iter().any(|breakdown| {
        breakdown.request_kind == UsageRequestKind::Compaction
            && breakdown.uncached_input_tokens == 50_000
    }));
    let plain = render_plain_with_cache(&SessionProjection::new(), 200_000, None, &fold);
    assert!(plain.contains("cache usage — logical"));
    assert!(plain.contains("/ Compaction —"));
    assert!(plain.contains("input $—"));

    fold.note(&usage(
        "unsupported",
        "ollama",
        UsageRequestKind::MainTurn,
        NormalizedUsage {
            logical_input: 100_000,
            uncached_input: 100_000,
            billed_output: 1_000,
            cache_status: CacheStatAvailability::Unavailable,
            cache_telemetry_input: 0,
            ..NormalizedUsage::default()
        },
    ));
    let mixed = fold.totals();
    assert_eq!(mixed.complete_hit_rate(), None);
    assert_eq!(
        wide_status(&mixed, None),
        "↑600k ↓229k ⚡108.8M re-read n/a"
    );
    let coverage = mixed.telemetry_coverage().expect("nonzero input");
    assert!(coverage < 1.0 && coverage > 0.0);

    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.cache_usage.note(&usage(
        "render",
        "openai",
        UsageRequestKind::MainTurn,
        present(450_000, 108_800_000, 227_000),
    ));
    let wide_frame = draw(&model, 180);
    assert!(
        wide_frame.contains("↑450k ↓227k") && wide_frame.contains("108.8M re-read n/a"),
        "wide status uses the exact form:\n{wide_frame}"
    );
    assert!(
        !draw(&model, 30).contains('⚡'),
        "narrow status drops the cache segment as a unit"
    );
}

/// A request kind unknown to this client has no classified meaning, so it is
/// visible as omitted telemetry but cannot enter known-lane totals or rates.
///
/// MUTATION `UNKNOWN_KIND_FOLDS_AS_MAIN_TURN`: change the wildcard in
/// `request_kind_rank` from `None` to `Some(0)`. The equality below must fail
/// because the unknown counters are then silently absorbed into the fold.
#[test]
fn unknown_request_kind_is_visible_but_excluded_from_classified_totals() {
    let mut fold = SessionUsageFold::default();
    fold.note(&usage(
        "known",
        "openai",
        UsageRequestKind::MainTurn,
        present(100, 900, 10),
    ));
    let classified = fold.totals();

    let mut unknown = usage(
        "future",
        "openai",
        UsageRequestKind::Unknown,
        present(9_000, 0, 500),
    );
    unknown.cache_cost = Some(haider_protocol::provider::CacheCostEstimate {
        input_with_cache_usd: 9.0,
        input_without_cache_usd: 10.0,
        estimated_savings_usd: 1.0,
        explicit_storage_usd: 0.0,
    });
    fold.note(&unknown);

    assert_eq!(fold.totals(), classified);
    assert_eq!(fold.totals().complete_hit_rate(), Some(0.9));
    assert!(fold.has_unclassified_usage());
    assert!(
        fold.totals()
            .breakdowns
            .iter()
            .all(|lane| lane.request_kind != UsageRequestKind::Unknown)
    );
    let plain = render_plain_with_cache(&SessionProjection::new(), 200_000, None, &fold);
    assert!(
        plain.contains("unclassified request usage present; excluded from totals and rates"),
        "{plain}"
    );
}

/// A published zero is a real 0.00% rate; an absent rate is n/a.
#[test]
fn cm1c_reported_zero_and_missing_render_differently() {
    let mut reported = SessionUsageFold::default();
    reported.note(&usage(
        "zero",
        "openai",
        UsageRequestKind::MainTurn,
        present(1_000, 0, 1),
    ));
    assert_eq!(
        wide_status(&reported.totals(), Some(0)),
        "↑1.0k ↓1 ⚡0 re-read 0.00%"
    );

    let mut missing = SessionUsageFold::default();
    missing.note(&usage(
        "missing",
        "ollama",
        UsageRequestKind::MainTurn,
        NormalizedUsage {
            logical_input: 1_000,
            uncached_input: 1_000,
            billed_output: 1,
            ..NormalizedUsage::default()
        },
    ));
    assert_eq!(
        wide_status(&missing.totals(), None),
        "↑1.0k ↓1 ⚡0 re-read n/a"
    );
}

#[test]
fn part_a_unknown_auth_is_unknown_price_not_a_subscription_plan() {
    let mut fold = SessionUsageFold::default();
    let mut unknown = usage(
        "unknown-auth",
        "compatible",
        UsageRequestKind::MainTurn,
        present(1_000, 0, 1),
    );
    unknown.scope.as_mut().expect("scope").auth_scope = "opaque".into();
    fold.note(&unknown);
    let plain = render_plain_with_cache(&SessionProjection::new(), 200_000, None, &fold);
    assert!(plain.contains("input $—"), "{plain}");
    assert!(!plain.contains(" · plan"), "{plain}");
}

/// LAW (Part A): OAuth lanes retain tokens/cache/hit-rate and carry a clearly
/// labeled API-rate equivalent. A mixed session keeps that equivalent
/// separate from the real API-key subtotal.
///
/// MUTATION CHECK (executed): classify `oauth_subscription` as API-key in
/// `scope_auth_method`; the separate real/equivalent ledgers fail.
#[test]
fn part_a_mixed_session_separates_metered_and_all_lane_api_rate() {
    let priced = |run: &str, epoch: &str, auth: &str, cost: f64| {
        let mut value = usage(
            run,
            "openai",
            UsageRequestKind::MainTurn,
            present(200, 800, 10),
        );
        let scope = value.scope.as_mut().expect("scope");
        scope.cache_epoch = epoch.into();
        scope.auth_scope = auth.into();
        value.cache_cost = Some(haider_protocol::provider::CacheCostEstimate {
            input_with_cache_usd: cost,
            input_without_cache_usd: cost * 2.0,
            estimated_savings_usd: cost,
            explicit_storage_usd: 0.0,
        });
        value
    };
    let mut fold = SessionUsageFold::default();
    fold.note(&priced("api", "api-epoch", "api_key", 0.25));
    fold.note(&priced("oauth", "oauth-epoch", "oauth_subscription", 9.99));
    let totals = fold.totals();
    assert_eq!(totals.logical_input_tokens, 2_000);
    assert_eq!(totals.cache_read_tokens, 1_600);
    assert_eq!(totals.complete_hit_rate(), Some(0.8));
    assert_eq!(totals.metered_input_tokens, 1_000);
    assert_eq!(totals.input_with_cache_usd, Some(0.25));
    let oauth = totals
        .breakdowns
        .iter()
        .find(|lane| lane.auth_method == Some(haider_protocol::credential::AuthMethod::OAuth))
        .expect("OAuth lane");
    assert_eq!(oauth.input_with_cache_usd, None);

    let plain = render_plain_with_cache(&SessionProjection::new(), 200_000, None, &fold);
    assert!(plain.contains("$0.2500"), "{plain}");
    assert!(plain.contains("metered lanes"), "{plain}");
    assert!(plain.contains("plan"), "{plain}");
    assert!(plain.contains("≈$10.2400/$20.4800 API rate"), "{plain}");
    assert!(plain.contains("≈$9.9900/$19.9800 API rate"), "{plain}");
    assert!(!plain.contains("input $10.2400"), "{plain}");
}

#[test]
fn part_a_mixed_unpriced_oauth_lane_keeps_labeled_unknown_all_lane_rate() {
    let mut api = usage(
        "api-known",
        "openai",
        UsageRequestKind::MainTurn,
        present(200, 800, 10),
    );
    api.scope.as_mut().expect("api scope").auth_scope = "api_key".into();
    api.cache_cost = Some(haider_protocol::provider::CacheCostEstimate {
        input_with_cache_usd: 0.25,
        input_without_cache_usd: 0.50,
        estimated_savings_usd: 0.25,
        explicit_storage_usd: 0.0,
    });
    let mut oauth = usage(
        "oauth-unknown",
        "unknown-provider",
        UsageRequestKind::DelegatedAgent,
        present(200, 800, 10),
    );
    let scope = oauth.scope.as_mut().expect("oauth scope");
    scope.auth_scope = "oauth_subscription".into();
    scope.model = "unknown-model".into();

    let mut model = AppModel::new();
    model.screen = Screen::Usage;
    model.cache_usage.note(&api);
    model.cache_usage.note(&oauth);
    let rich = draw(&model, 180);
    assert!(rich.contains("$0.2500"), "{rich}");
    assert!(rich.contains("$— API rate (all lanes)"), "{rich}");
    assert!(rich.contains("plan · input $— API rate"), "{rich}");

    let plain =
        render_plain_with_cache(&SessionProjection::new(), 200_000, None, &model.cache_usage);
    assert!(plain.contains("$— API rate (all lanes)"), "{plain}");
    assert!(plain.contains("plan · input $— API rate"), "{plain}");
}
