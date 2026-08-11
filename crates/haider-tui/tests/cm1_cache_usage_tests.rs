#![allow(clippy::expect_used)]

use haider_protocol::ids::RunId;
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
            cache_epoch: "epoch-fixture".into(),
            stable_prefix_tokens: 0,
            cache_boundaries: None,
            request_kind: kind,
            run: Some(RunId::new(run)),
            agent: None,
            prefix_digests: None,
        }),
        cache_cost: None,
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
    assert_eq!(wide_status(&totals), "↑450k ↓227k ⚡108.8M 99.59% hit");
    assert_eq!(medium_status(&totals), "⚡108.8M 99.6% hit");

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
    assert_eq!(wide_status(&mixed), "⚡n/a · hit n/a");
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
        wide_frame.contains("↑450k ↓227k") && wide_frame.contains("108.8M 99.59% hit"),
        "wide status uses the exact form:\n{wide_frame}"
    );
    assert!(
        !draw(&model, 30).contains('⚡'),
        "narrow status drops the cache segment as a unit"
    );
}

/// A reported zero is a real 0.00% rate; missing telemetry is n/a.
#[test]
fn cm1c_reported_zero_and_missing_render_differently() {
    let mut reported = SessionUsageFold::default();
    reported.note(&usage(
        "zero",
        "openai",
        UsageRequestKind::MainTurn,
        present(1_000, 0, 1),
    ));
    assert_eq!(wide_status(&reported.totals()), "↑1.0k ↓1 ⚡0 0.00% hit");

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
    assert_eq!(wide_status(&missing.totals()), "⚡n/a · hit n/a");
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

/// LAW (Part A): OAuth lanes retain tokens/cache/hit-rate but contribute no
/// dollar estimate. A mixed session renders only the API-key subtotal and
/// labels it as the metered portion.
///
/// MUTATION CHECK (executed): classify `oauth_subscription` as API-key in
/// `scope_auth_method`; the exact subtotal and no-dollar plan lane fail.
#[test]
fn part_a_mixed_session_displays_only_metered_lane_cost() {
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
    assert!(!plain.contains("$9.9900"), "{plain}");
}
