#![allow(clippy::expect_used)]

use super::*;
#[test]
fn blocking_wait_requires_parent_report_even_after_child_terminal() {
    let agent = AgentId::new("agent-race");
    let terminal = RunState::Errored;
    // Inject the journal ordering: the child is terminal, but the parent
    // has not committed ChildResult yet. No clock or process race needed.
    assert!(select_wait_report(&agent, Some(&terminal), "", None, true, false).is_none());
    let durable = ChildReport {
        agent: agent.clone(),
        summary: "failed".into(),
        verified: ReportVerification::Red,
        workspace_revision: None,
    };
    assert_eq!(
        select_wait_report(
            &agent,
            Some(&terminal),
            "",
            Some(&(durable.clone(), 17)),
            true,
            false
        ),
        Some((durable, Some(17)))
    );
}

#[test]
fn journal_wait_marks_failed_and_cancelled_children_red() {
    let agent = AgentId::new("agent-journal");
    for state in [RunState::Errored, RunState::Cancelled] {
        let (report, seq) = select_wait_report(&agent, Some(&state), "failed", None, true, true)
            .expect("background journal report");
        assert_eq!(report.verified, ReportVerification::Red);
        assert_eq!(seq, None);
    }
}
