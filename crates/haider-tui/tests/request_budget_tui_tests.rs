#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::{ItemId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::request_budget::{
    PROVIDER_REQUEST_BUDGET_EXTENSION_KIND, RequestBudgetContinuationV1, RequestBudgetPhaseV1,
    RequestBudgetStatusV1, RequestBudgetV1,
};
use haider_tui::app::{AppEvent, AppModel};
use haider_tui::mock::demo_script;
use haider_tui::plain::render_plain;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[test]
fn budget_status_without_label_renders_counts_and_continuation_in_both_surfaces() {
    for (phase, used) in [
        (RequestBudgetPhaseV1::Progress, 31),
        (RequestBudgetPhaseV1::SoftBound, 32),
        (RequestBudgetPhaseV1::HardBound, 64),
    ] {
        let mut model = AppModel::new();
        for payload in demo_script() {
            model.handle(AppEvent::Envelope(Box::new(payload)));
        }
        let status = RequestBudgetStatusV1 {
            used,
            budget: RequestBudgetV1::default(),
            phase,
            continuation: RequestBudgetContinuationV1 {
                session_id: SessionId::new("budget-session"),
                run_id: RunId::new("budget-run"),
                branch_id: None,
                agent_id: None,
            },
        };
        model
            .projection
            .apply(&EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("budget-progress"),
                item: TurnItem::Extension {
                    kind: PROVIDER_REQUEST_BUDGET_EXTENSION_KIND.into(),
                    data: serde_json::to_value(&status).expect("typed budget"),
                },
            }));
        let expected = format!("requests {used} / tranche 32 / hard cap 64");
        let plain = render_plain(&model.projection, 0, None);
        assert!(plain.contains(&expected), "plain: {plain}");
        if phase != RequestBudgetPhaseV1::Progress {
            assert!(plain.contains("resume budget-run"));
        }
        let mut terminal = Terminal::new(TestBackend::new(180, 40)).expect("terminal");
        terminal
            .draw(|frame| {
                render(&model, frame);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(&expected), "styled: {rendered}");
    }
}
