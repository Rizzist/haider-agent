//! `/usage` → calendar: provider reset instants on a month grid (970),
//! reworked for clarity in 973:
//!
//! - accounts list in the ONE on-screen order ([`UsageState::display_order`])
//!   the arrow keys step through;
//! - every reset reads `local (UTC) · relative`, the grid's days are local
//!   days when the host zone is known (UTC otherwise, and the header says
//!   which);
//! - the selected account's 5h and weekly reset days are highlighted on the
//!   grid in a style distinct from today, other accounts' markers dim;
//! - the selected account opens a DETAIL panel ([`crate::usage_detail`]).
//!
//! The report timestamp is the sole `today` authority and relative deltas
//! are measured from it, so a frozen fixture renders byte-for-byte stable
//! frames. Unknown is never inferred: a missing window or reset says so.
//!
//! [`UsageState::display_order`]: crate::app::UsageState::display_order

use haider_protocol::usage::{AccountMeterStateV1, AccountUsageReportV1, UsageWindowV1};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{ellipsize, hover_band};
use crate::app::AppModel;
use crate::theme::Theme;
use crate::usage_detail::{
    DetailTone, account_extras, detail_rows, fmt_meter_reason_friendly, is_haider_code,
};
use crate::usage_time::{
    UsageTimeZone, fmt_relative, fmt_reset_instant, fmt_reset_short, fmt_zone_caption, zoned_day,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResetKind {
    FiveHour,
    Weekly,
}

impl ResetKind {
    const BOTH: [Self; 2] = [Self::FiveHour, Self::Weekly];

    const fn marker(self) -> char {
        match self {
            Self::FiveHour => '5',
            Self::Weekly => 'W',
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::Weekly => "weekly",
        }
    }
}

/// Pick only provider-published windows whose names have a defined meaning.
/// In particular, this never derives a reset by adding five hours or seven
/// days: a missing canonical window/reset stays `reset unknown`. Haider Code
/// publishes one account-wide weekly allowance and no five-hour window.
fn reset_window(account: &AccountUsageReportV1, kind: ResetKind) -> Option<&UsageWindowV1> {
    let AccountMeterStateV1::Metered { windows } = &account.meter else {
        return None;
    };
    let provider = account.provider.to_ascii_lowercase();
    let names: &[&str] = match (provider.as_str(), kind) {
        (provider, ResetKind::FiveHour) if provider.starts_with("anthropic") => &["five_hour"],
        (provider, ResetKind::Weekly) if provider.starts_with("anthropic") => &["seven_day"],
        (provider, ResetKind::FiveHour)
            if provider.starts_with("openai") || provider.contains("codex") =>
        {
            &["5h", "five_hour"]
        }
        (provider, ResetKind::Weekly)
            if provider.starts_with("openai") || provider.contains("codex") =>
        {
            &["weekly", "seven_day"]
        }
        (provider, ResetKind::Weekly) if is_haider_code(provider) => &["weekly", "seven_day"],
        _ => return None,
    };
    // OpenAI can publish identically sized named per-model windows. Only an
    // unlabeled provider window is account-wide; even a lone labeled window
    // is not a safe account reset and must stay unknown.
    windows
        .iter()
        .find(|window| window.label.is_none() && names.contains(&window.window.as_str()))
}

fn reset_at(account: &AccountUsageReportV1, kind: ResetKind) -> Option<u64> {
    reset_window(account, kind).and_then(|window| window.resets_at_ms)
}

fn account_marker(index: usize) -> String {
    u8::try_from(index)
        .ok()
        .filter(|index| *index < 26)
        .map_or_else(
            || (index + 1).to_string(),
            |index| char::from(b'a' + index).to_string(),
        )
}

fn month_name(month: u32) -> &'static str {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    month
        .checked_sub(1)
        .and_then(|month| usize::try_from(month).ok())
        .and_then(|month| MONTHS.get(month))
        .copied()
        .unwrap_or("Unknown")
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = crate::format::days_from_civil(year, month, 1);
    let next = crate::format::days_from_civil(next_year, next_month, 1);
    u32::try_from(next.saturating_sub(first)).unwrap_or(31)
}

fn bold(style: Style) -> Style {
    style.add_modifier(Modifier::BOLD)
}

/// Today's cell: the gold soft ground (unchanged from 970).
fn today_style(theme: &Theme) -> Style {
    bold(theme.gold_style().bg(theme.gold_soft.into()))
}

/// A reset day of the SELECTED account: a filled badge, unmistakably not
/// "today" (gold) and not a hover band.
fn selected_day_style(theme: &Theme) -> Style {
    bold(
        Style::default()
            .fg(theme.badge_fg.into())
            .bg(theme.ok.into()),
    )
}

fn detail_style(theme: &Theme, tone: DetailTone) -> Style {
    match tone {
        DetailTone::Normal => theme.text_style(),
        DetailTone::Dim => theme.dim_style(),
        DetailTone::Warn => theme.warn_style(),
        DetailTone::Action => theme.gold_style(),
        DetailTone::Disabled => theme.faint_style(),
    }
}

/// One plotted reset: (grid day, display index, kind).
type Event = (i64, usize, ResetKind);

/// The one-line answer to "which account, and when": the selected account's
/// resets, relative first.
fn selection_summary(
    account: &AccountUsageReportV1,
    marker: &str,
    snapshot_ms: u64,
    zone: UsageTimeZone,
    year: i64,
) -> String {
    let parts = ResetKind::BOTH
        .iter()
        .map(|&kind| {
            reset_at(account, kind).map_or_else(
                || format!("{} unknown", kind.label()),
                |at| {
                    format!(
                        "{} {} ({})",
                        kind.label(),
                        fmt_relative(snapshot_ms, at),
                        fmt_reset_short(at, zone, year)
                    )
                },
            )
        })
        .collect::<Vec<_>>();
    format!(
        "  selected {marker} {} — {}",
        account.alias.as_str(),
        parts.join(" · ")
    )
}

/// The provider reset calendar. See the module docs.
pub(super) fn render_usage_calendar(
    model: &AppModel,
    theme: &Theme,
    area_width: u16,
    lines: &mut Vec<Line<'_>>,
    header_lines: &mut Vec<usize>,
    follow_end: &mut Option<usize>,
) {
    let Some(report) = &model.usage.report else {
        if !model.usage.fetching && model.usage.error.is_none() {
            lines.push(Line::styled(
                "  no usage snapshot yet — reset calendar unavailable",
                theme.dim_style(),
            ));
        }
        return;
    };

    let order = model
        .usage
        .display_order()
        .into_iter()
        .filter(|&slot| slot < report.accounts.len())
        .collect::<Vec<_>>();
    if order.is_empty() {
        lines.push(Line::styled(
            if report.accounts.is_empty() {
                "  no accounts known — /login adds one".to_owned()
            } else {
                format!(
                    "  no accounts match \"{}\" — bare /usage clears the filter",
                    model.usage.filter.as_deref().unwrap_or_default()
                )
            },
            theme.dim_style(),
        ));
        return;
    }

    let zone = model.usage.time_zone;
    let snapshot_ms = report.generated_at_ms;
    let local = zone.offset_at_ms(snapshot_ms).is_some();
    let today = zoned_day(snapshot_ms, zone);
    let (year, month, today_date) = crate::format::civil_from_days(today);
    let month_start = crate::format::days_from_civil(year, month, 1);
    let grid_start = month_start - i64::from(crate::format::weekday_from_days(month_start));
    let month_end = month_start + i64::from(days_in_month(year, month)) - 1;
    let required_end = month_end.max(today.saturating_add(14));
    let grid_end = required_end
        + i64::from(6_u32.saturating_sub(crate::format::weekday_from_days(required_end)));

    let selected_slot = model.usage.selected_slot();
    let selected_index = selected_slot.and_then(|slot| order.iter().position(|&s| s == slot));
    let mut events: Vec<Event> = Vec::new();
    for (index, &slot) in order.iter().enumerate() {
        let account = &report.accounts[slot];
        for kind in ResetKind::BOTH {
            if let Some(at) = reset_at(account, kind) {
                events.push((zoned_day(at, zone), index, kind));
            }
        }
    }
    let selected_days = events
        .iter()
        .filter(|(_, index, _)| Some(*index) == selected_index)
        .map(|(day, _, _)| *day)
        .collect::<Vec<_>>();

    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} {year:04}", month_name(month)),
            bold(theme.bright_style()),
        ),
        Span::styled(
            if local && area_width >= 100 {
                format!(
                    " · {} calendar · UTC beside every time · provider timestamps only",
                    fmt_zone_caption(snapshot_ms, zone)
                )
            } else if local {
                format!(
                    " · {} days · UTC shown too",
                    fmt_zone_caption(snapshot_ms, zone)
                )
            } else {
                format!(
                    " · {} reset calendar · provider timestamps only",
                    fmt_zone_caption(snapshot_ms, zone)
                )
            },
            theme.dim_style(),
        ),
    ]));
    let budget = usize::from(area_width).saturating_sub(1);
    lines.push(Line::from(vec![
        Span::styled(format!("  [{today_date:02}] today · "), theme.dim_style()),
        Span::styled("  ", selected_day_style(theme)),
        Span::styled(
            " selected account's resets · others dim · ↑↓ ←→ step the list",
            theme.dim_style(),
        ),
    ]));
    if let (Some(slot), Some(index)) = (selected_slot, selected_index) {
        lines.push(Line::styled(
            ellipsize(
                &selection_summary(
                    &report.accounts[slot],
                    &account_marker(index),
                    snapshot_ms,
                    zone,
                    year,
                ),
                budget,
            ),
            theme.gold_style(),
        ));
    }
    lines.push(Line::raw(""));

    let cell_width = usize::from(area_width.saturating_sub(2) / 7).max(7);
    let mut weekday_line = vec![Span::raw("  ")];
    for name in ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"] {
        weekday_line.push(Span::styled(
            format!("{name:<cell_width$}"),
            theme.dim_style(),
        ));
    }
    lines.push(Line::from(weekday_line));

    let mut week_start = grid_start;
    while week_start <= grid_end {
        let mut week = vec![Span::raw("  ")];
        for offset in 0_i64..7 {
            let day = week_start + offset;
            week.extend(grid_cell(
                theme,
                day,
                today,
                month,
                cell_width,
                &events,
                selected_index,
                selected_days.contains(&day),
            ));
        }
        lines.push(Line::from(week));
        week_start += 7;
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("  RESET MARKERS", bold(theme.bright_style())),
        Span::styled(
            " — exact meter fields · unknown is never inferred",
            theme.dim_style(),
        ),
    ]));
    let wide = area_width >= 100;
    let mut account_lines = vec![None; report.accounts.len()];
    for (index, &slot) in order.iter().enumerate() {
        let account = &report.accounts[slot];
        account_lines[slot] = Some(lines.len());
        let selected = selected_slot == Some(slot);
        push_account_rows(
            lines,
            theme,
            account,
            &account_marker(index),
            selected,
            wide,
            area_width,
            zone,
            snapshot_ms,
            year,
        );
        if selected {
            *follow_end = Some(lines.len().saturating_sub(1));
        }
    }
    for group in &model.usage.groups() {
        let selected = group
            .accounts
            .get(model.usage.selected_tab(group))
            .copied()
            .or_else(|| group.accounts.first().copied());
        header_lines.push(
            selected
                .and_then(|slot| account_lines.get(slot).copied().flatten())
                .unwrap_or(0),
        );
    }
}

/// One grid cell as styled spans: the date (bracketed when today) and each
/// account marker, the selected account's in the cell style, others dim.
#[allow(clippy::too_many_arguments)]
fn grid_cell(
    theme: &Theme,
    day: i64,
    today: i64,
    month: u32,
    cell_width: usize,
    events: &[Event],
    selected_index: Option<usize>,
    selected_day: bool,
) -> Vec<Span<'static>> {
    let (_, cell_month, date) = crate::format::civil_from_days(day);
    let date = if day == today {
        format!("[{date:02}]")
    } else {
        format!(" {date:02} ")
    };
    let cell_style = if selected_day {
        selected_day_style(theme)
    } else if day == today {
        today_style(theme)
    } else if cell_month == month {
        theme.text_style()
    } else {
        theme.faint_style()
    };
    let markers = events
        .iter()
        .filter(|(event_day, _, _)| *event_day == day)
        .map(|(_, index, kind)| {
            (
                format!("{}{}", account_marker(*index), kind.marker()),
                *index,
            )
        })
        .collect::<Vec<_>>();
    let text_len = date.chars().count()
        + markers
            .iter()
            .map(|(marker, _)| marker.chars().count() + 1)
            .sum::<usize>();
    if text_len > cell_width {
        // Too many markers for the cell: one ellipsized run in cell style.
        let text = format!(
            "{date} {}",
            markers
                .iter()
                .map(|(marker, _)| marker.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        return vec![Span::styled(
            format!("{:<cell_width$}", ellipsize(&text, cell_width)),
            cell_style,
        )];
    }
    let mut spans = vec![Span::styled(date, cell_style)];
    for (position, (marker, index)) in markers.iter().enumerate() {
        spans.push(Span::styled(
            if position == 0 { " " } else { "," },
            cell_style,
        ));
        let style = if Some(*index) == selected_index {
            if selected_day {
                cell_style
            } else {
                bold(theme.gold_style())
            }
        } else if selected_day {
            // Another account sharing the selected day: keep the badge
            // ground so the day reads as one block, drop the weight.
            cell_style.remove_modifier(Modifier::BOLD)
        } else if day == today {
            theme.dim_style().bg(theme.gold_soft.into())
        } else {
            theme.dim_style()
        };
        spans.push(Span::styled(marker.clone(), style));
    }
    let pad = cell_width.saturating_sub(text_len);
    if pad > 0 {
        spans.push(Span::styled(
            " ".repeat(pad),
            if selected_day || day == today {
                cell_style
            } else {
                Style::default()
            },
        ));
    }
    spans
}

/// The account's list rows: the identity header, one row per reset kind
/// (or the typed unavailability), then — for the selected account — the
/// detail panel.
#[allow(clippy::too_many_arguments)]
fn push_account_rows(
    lines: &mut Vec<Line<'_>>,
    theme: &Theme,
    account: &AccountUsageReportV1,
    marker: &str,
    selected: bool,
    wide: bool,
    area_width: u16,
    zone: UsageTimeZone,
    snapshot_ms: u64,
    year: i64,
) {
    let head = if wide {
        let identity = account
            .identity
            .as_deref()
            .map(crate::format::mask_identity)
            .unwrap_or_else(|| "—".to_owned());
        let plan = account.plan.as_deref().unwrap_or("plan unknown");
        format!(
            "  {} {marker}  {} · {} · {identity} · {plan}",
            if selected { ">" } else { " " },
            account.alias.as_str(),
            account.provider,
        )
    } else {
        format!(
            "  {} {marker}  {} · {}",
            if selected { ">" } else { " " },
            account.alias.as_str(),
            account.provider,
        )
    };
    // A span (not line) style, so the selection band below keeps the ink.
    let head = Line::from(Span::styled(
        head,
        if selected {
            bold(theme.gold_style())
        } else {
            theme.bright_style()
        },
    ));
    lines.push(hover_band(head, selected, area_width, theme));

    match &account.meter {
        AccountMeterStateV1::Unavailable { reason } => lines.push(Line::styled(
            if wide {
                format!(
                    "      5h reset unknown · weekly reset unknown · meter unavailable · {}",
                    fmt_meter_reason_friendly(reason)
                )
            } else {
                // 80 columns: the reason is the actionable part — lead with
                // it and ellipsize visibly instead of clipping mid-word.
                ellipsize(
                    &format!(
                        "      resets unknown · meter unavailable · {}",
                        fmt_meter_reason_friendly(reason)
                    ),
                    usize::from(area_width).saturating_sub(1),
                )
            },
            theme.warn_style(),
        )),
        AccountMeterStateV1::LocalOnly => lines.push(Line::styled(
            "      5h reset unknown · weekly reset unknown · local only; no provider meter",
            theme.dim_style(),
        )),
        AccountMeterStateV1::Metered { .. } => {
            for kind in ResetKind::BOTH {
                let row = reset_window(account, kind).map_or_else(
                    || {
                        format!(
                            "      {} reset unknown · exact window not published",
                            kind.label()
                        )
                    },
                    |window| {
                        window.resets_at_ms.map_or_else(
                            || {
                                format!(
                                    "      {} reset unknown · {} published no reset",
                                    kind.label(),
                                    window.window
                                )
                            },
                            |at| {
                                let when = format!(
                                    "      {} {} · {}",
                                    kind.label(),
                                    fmt_reset_instant(at, zone, year),
                                    fmt_relative(snapshot_ms, at)
                                );
                                if wide {
                                    format!("{when} · {}", window.window)
                                } else {
                                    when
                                }
                            },
                        )
                    },
                );
                lines.push(Line::styled(
                    row,
                    if selected {
                        theme.text_style()
                    } else {
                        theme.dim_style()
                    },
                ));
            }
        }
    }

    if selected {
        let extras = account_extras(account);
        let label_width = 17;
        lines.push(Line::from(vec![
            Span::styled("      │ ", theme.faint_style()),
            Span::styled("DETAIL", bold(theme.bright_style())),
            Span::styled(
                format!(" {} — provider-reported extras", account.alias.as_str()),
                theme.dim_style(),
            ),
        ]));
        for row in detail_rows(account, &extras, zone, snapshot_ms, year) {
            lines.push(Line::from(vec![
                Span::styled("      │ ", theme.faint_style()),
                Span::styled(format!("{:<label_width$}", row.label), theme.dim_style()),
                Span::styled(
                    ellipsize(
                        &row.text,
                        usize::from(area_width).saturating_sub(8 + label_width + 1),
                    ),
                    detail_style(theme, row.tone),
                ),
            ]));
        }
    }
}
