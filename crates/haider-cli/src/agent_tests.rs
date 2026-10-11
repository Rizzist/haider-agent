#![allow(clippy::expect_used)]

use super::*;
#[test]
fn helper_stop_remedy_respects_parsed_cancel_mode() {
    for target in [vec!["parent", "child"], vec!["parent", "--all"]] {
        for no_spawn in [false, true] {
            for json in [false, true] {
                for flags_first in [false, true] {
                    let flags: Vec<_> =
                        [no_spawn.then_some("--no-spawn"), json.then_some("--json")]
                            .into_iter()
                            .flatten()
                            .collect();
                    let args: Vec<_> = if flags_first {
                        flags.iter().chain(&target)
                    } else {
                        target.iter().chain(&flags)
                    }
                    .map(|arg| (*arg).to_owned())
                    .collect();
                    let mut argv = vec!["cancel".to_owned()];
                    argv.extend(args);
                    let options = Options::parse("agent", &argv)
                        .unwrap_or_else(|_| panic!("cancel argv: {argv:?}"));
                    assert_eq!(options.no_spawn, no_spawn);
                    let message = helper_stop_remedy(options.no_spawn);
                    assert!(message.contains("`haider daemon stop`"));
                    if no_spawn {
                        assert!(
                            message
                                .ends_with("without --no-spawn so the current daemon can start.")
                        );
                    } else {
                        assert!(message.ends_with("the current daemon starts automatically."));
                    }
                }
            }
        }
    }
}

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

#[test]
fn agent_spawn_derives_output_budget_for_a_small_limit_model() {
    let spec = AgentSpawnSpecV1 {
        task: "test".into(),
        prompt: "hello".into(),
        provider: Some("openai".into()),
        model: Some("gpt-4o".into()),
        agent_type: None,
        workflow: None,
        workflow_trigger: None,
    };
    let request = agent_session_create_request("agent-test", "/tmp".into(), &spec);
    assert!(
        matches!(request, RequestBody::SessionCreateWithPermissionOverrides {
        max_tokens: 0, provider, model, ..
    } if provider == "openai" && model == "gpt-4o")
    );
}

#[test]
fn workflow_run_uses_the_same_derived_budget() {
    let spec = AgentSpawnSpecV1 {
        task: "workflow".into(),
        prompt: "hello".into(),
        provider: Some("deepseek".into()),
        model: Some("deepseek-reasoner".into()),
        agent_type: None,
        workflow: Some("audit".into()),
        workflow_trigger: Some("manual".into()),
    };
    assert!(matches!(
        agent_session_create_request("workflow-test", "/tmp".into(), &spec),
        RequestBody::SessionCreateWithPermissionOverrides { max_tokens: 0, .. }
    ));
}
