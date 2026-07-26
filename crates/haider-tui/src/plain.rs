//! `--plain` renderer: the projection as plain UTF-8 lines — no colors, no
//! cursor control, no alternate screen. This is the CI/test/pipe fallback
//! (and the oracle for "did the projection say the right thing"), so shapes
//! stay stable and greppable.

use crate::format::{METER_CELLS_DEFAULT, fmt_tok, meter_cells};
use crate::projection::{ItemBlock, SessionProjection, TranscriptEntry};
use haider_protocol::item::{ToolStatus, TurnItem};

/// Status glyphs shared by tool and command rows (sim chip vocabulary).
#[must_use]
pub const fn status_glyph(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "…",
        ToolStatus::InProgress => "◌",
        ToolStatus::Completed => "✓",
        ToolStatus::Failed => "✗",
        ToolStatus::Cancelled => "⊘",
    }
}

/// Render the whole session view as plain lines: transcript, pinned todos,
/// open menu, status line. `window` sizes the context meter (0 = no meter).
#[must_use]
pub fn render_plain(projection: &SessionProjection, window: u64) -> String {
    let mut out = String::new();
    for entry in projection.entries() {
        match entry {
            TranscriptEntry::User { text, attachments } => {
                out.push_str("❯ ");
                out.push_str(text);
                if *attachments > 0 {
                    out.push_str(&format!(" [+{attachments} attachment(s)]"));
                }
                out.push('\n');
            }
            TranscriptEntry::Item(block) => render_item(&mut out, block),
        }
    }
    if let Some(todos) = projection.todos().filter(|t| t.pinned) {
        out.push_str(&format!(
            "todos — {}/{} done",
            todos.done_count(),
            todos.items.len()
        ));
        if let Some(current) = todos.current() {
            out.push_str(&format!(" · ■ {}", current.text));
        }
        out.push('\n');
        for item in &todos.items {
            let mark = match item.state {
                haider_protocol::history::TodoState::Completed => "✓",
                haider_protocol::history::TodoState::Processing => "■",
                haider_protocol::history::TodoState::Listed => "☐",
            };
            out.push_str(&format!("  {mark} {}\n", item.text));
        }
    }
    if let Some(menu) = projection.open_menu() {
        out.push_str(&format!("? {}\n", menu.title));
        for (index, option) in menu.options.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", index + 1, option.label));
        }
    }
    out.push_str(&status_line(projection, window));
    out.push('\n');
    out
}

/// The plain status line: `IDLE · 33k tok · ▰▰▱▱▱▱▱▱▱▱ 17% of 200k`.
#[must_use]
pub fn status_line(projection: &SessionProjection, window: u64) -> String {
    let badge = projection.badge();
    let tokens = projection.context_tokens();
    if window == 0 {
        return format!("{badge} · {} tok", fmt_tok(tokens));
    }
    #[allow(clippy::cast_precision_loss)]
    let pct = tokens as f64 / window as f64;
    format!(
        "{badge} · {} tok · {} {}% of {}",
        fmt_tok(tokens),
        meter_cells(pct, METER_CELLS_DEFAULT),
        (pct.clamp(0.0, 1.0) * 100.0).round(),
        fmt_tok(window)
    )
}

fn render_item(out: &mut String, block: &ItemBlock) {
    match &block.item {
        TurnItem::AgentMessage { text } => {
            out.push_str(text);
            if block.streaming {
                out.push('▮');
            }
            out.push('\n');
        }
        TurnItem::Reasoning { summary } => {
            out.push_str("· ");
            out.push_str(summary);
            out.push('\n');
        }
        TurnItem::ToolCall { name, status, .. } => {
            out.push_str(&format!("⚒ {name} {}\n", status_glyph(*status)));
        }
        TurnItem::CommandExecution {
            command,
            status,
            exit_code,
            ..
        } => {
            out.push_str(&format!("$ {command} {}", status_glyph(*status)));
            if let Some(code) = exit_code {
                out.push_str(&format!(" · exit {code}"));
            }
            out.push('\n');
            if block.output_truncated {
                out.push_str("  ⋯ earlier output truncated\n");
            }
            if block.output_decode_error {
                out.push_str("  ⚠ some output could not be decoded\n");
            }
            let tail = block.output_text();
            for line in tail.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
        TurnItem::FileChange {
            path,
            added,
            removed,
        } => {
            out.push_str(&format!("± {path} +{added} -{removed}\n"));
        }
        TurnItem::ChildSpawn { agent } => {
            out.push_str(&format!("◉ subagent {} spawned\n", agent.as_str()));
        }
        TurnItem::ChildResult { report } => {
            out.push_str(&format!("└ subagent report — {}\n", report.summary));
        }
        TurnItem::Plan { items } => {
            let done = items
                .iter()
                .filter(|i| i.state == haider_protocol::history::TodoState::Completed)
                .count();
            out.push_str(&format!("✓ plan — {done}/{} done\n", items.len()));
        }
        TurnItem::ContextCompaction { .. } => out.push_str("⊟ context compacted\n"),
        TurnItem::Extension { kind, .. } => out.push_str(&format!("⋯ {kind}\n")),
    }
}
