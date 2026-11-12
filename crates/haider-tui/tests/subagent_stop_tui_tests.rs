#![allow(clippy::expect_used)]
//! Owner decision 2026-09-25: Esc stops the main thread only. The durable
//! still-running marker and a detached child's later-delivered result must be
//! visible, honest transcript rows in both the styled and plain surfaces.

use haider_protocol::EventPayload;
use haider_protocol::agent::ReportVerification;
use haider_protocol::ids::{AgentId, ItemId, RunId, SessionId};
use haider_protocol::item::ItemEvent;
use haider_protocol::subagent_stop::{
    DetachedChildResultV1, DetachedChildStateV1, StillRunningAgentV1, SubagentsStillRunningV1,
};
use haider_tui::app::{AppEvent, AppModel};
use haider_tui::mock::demo_script;
use haider_tui::plain::render_plain;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn styled(model: &AppModel) -> String {
    let mut terminal = Terminal::new(TestBackend::new(200, 50)).expect("terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
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

/// MUTATION CHECK: drop the marker branch from either renderer (the generic
/// extension row shows only a faint label) or change the summary wording.
/// Expected runtime failure: the "still running" sentence or the stop-all
/// command is missing from that surface.
#[test]
fn main_thread_stop_marker_and_delivered_result_render_in_both_surfaces() {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let marker = SubagentsStillRunningV1 {
        parent_session_id: SessionId::new("sess-root"),
        stopped_run_id: RunId::new("run-root"),
        still_running: 2,
        agents: vec![
            StillRunningAgentV1 {
                agent: AgentId::new("agent-b"),
                task: "background b".into(),
                result_pending: false,
            },
            StillRunningAgentV1 {
                agent: AgentId::new("agent-c"),
                task: "waited c".into(),
                result_pending: true,
            },
        ],
        agents_truncated: false,
    };
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("still-running"),
            item: marker.to_extension_item().expect("marker item"),
        }));
    let delivered = DetachedChildResultV1 {
        agent: AgentId::new("agent-c"),
        task: "waited c".into(),
        stopped_run_id: RunId::new("run-root"),
        state: DetachedChildStateV1::Done,
        verified: ReportVerification::Unverified,
        summary: "c finished".into(),
        summary_truncated: false,
        reason: Default::default(),
    };
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("delivered"),
            item: delivered.to_extension_item().expect("delivered item"),
        }));
    let marker_line = "main thread stopped — 2 subagents still running";
    let delivered_line = "subagent agent-c (waited c) finished after its waiting turn was stopped";
    let plain = render_plain(&model.projection, 0, None);
    assert!(
        plain.contains(&format!("⧗ {marker_line}")),
        "plain: {plain}"
    );
    assert!(
        plain.contains("1 waited-on result will be delivered when finished"),
        "plain: {plain}"
    );
    assert!(
        plain.contains("haider agent cancel sess-root --all --json"),
        "plain: {plain}"
    );
    assert!(
        plain.contains(&format!("↩ {delivered_line}")),
        "plain: {plain}"
    );
    let rendered = styled(&model);
    assert!(rendered.contains(marker_line), "styled: {rendered}");
    assert!(rendered.contains(delivered_line), "styled: {rendered}");
}
