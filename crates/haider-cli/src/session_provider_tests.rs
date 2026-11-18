use super::*;
#[test]
fn active_rebind_notice_reports_only_the_active_clamp() {
    use haider_protocol::output_budget::{
        OutputBudgetClampV1, SessionOutputBudgetSourceV1, SessionOutputBudgetV1,
    };
    let mut active = haider_protocol::session::ActiveRunOutputBudgetV1 {
        run_id: haider_protocol::ids::RunId::new("run-b"),
        model: "small".into(),
        budget: SessionOutputBudgetV1 {
            max_tokens: 8192,
            source: SessionOutputBudgetSourceV1::UserSet { requested: 20000 },
            clamped: Some(OutputBudgetClampV1 {
                requested: 20000,
                max_output_tokens: 8192,
            }),
        },
    };
    assert_eq!(active_rebind_notice(Some(&active)), Some("haider session provider rebind: active run run-b (small): Output budget 20000 exceeds this model's maximum; using 8192 per response. Switching to a larger model restores 20000.".into()));
    active.model = "wide".into();
    active.budget.clamped = None;
    assert_eq!(active_rebind_notice(Some(&active)), None);
    assert_eq!(active_rebind_notice(None), None);
}
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}
#[test]
fn provider_rebind_cli_requires_explicit_target_and_rejects_ambiguous_flags() {
    for invalid in [
        vec![],
        vec!["--session", "a"],
        vec!["--provider", "p"],
        vec!["--session", "a", "--provider", "p", "--account"],
        vec!["--session", "a", "--provider", "p", "--session", "b"],
    ] {
        assert!(parse(&args(&invalid)).is_err(), "{invalid:?}");
    }
    assert_eq!(
        parse(&args(&[
            "--session",
            "a",
            "--provider",
            "proxy",
            "--base-url",
            "http://127.0.0.1:8000",
            "--account",
            "bench"
        ])),
        Ok(Options {
            session: "a".into(),
            provider: "proxy".into(),
            base_url: Some("http://127.0.0.1:8000".into()),
            account: Some("bench".into())
        })
    );
}
