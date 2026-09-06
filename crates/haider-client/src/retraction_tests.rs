use super::*;

#[test]
fn too_late_fallback_retains_run_and_command_across_retries() {
    let mut request = TurnRetraction::new(
        SessionId::new("session"),
        RunId::new("accepted-run"),
        9,
        CommandId::new("retract-command"),
    );
    assert!(matches!(
        request.apply_response(ResponseBody::Error {
            code: haider_rpc::ERROR_CODE_TOO_LATE.into(),
            message: "response arrived".into(),
            retryable: false,
            data: None,
        }),
        Ok(None)
    ));
    for _ in 0..2 {
        assert!(matches!(
            request.request_body(),
            RequestBody::TurnCancel {
                command_id,
                session_id,
                worker_generation: 9,
                run_id,
            } if command_id.as_str() == "retract-command:cancel"
                && session_id.as_str() == "session"
                && run_id.as_str() == "accepted-run"
        ));
    }
    assert!(
        request
            .apply_response(ResponseBody::TurnCancel {
                session_id: SessionId::new("session"),
                run_id: RunId::new("replacement-run"),
                status: CancelStatus::Accepted,
                terminal_seq: None,
            })
            .is_err()
    );
    assert!(matches!(
        request.apply_response(ResponseBody::TurnCancel {
            session_id: SessionId::new("session"),
            run_id: RunId::new("accepted-run"),
            status: CancelStatus::AlreadyTerminal,
            terminal_seq: Some(12),
        }),
        Ok(Some(TurnRetractionOutcome::Cancelled {
            terminal_seq: Some(12),
            ..
        }))
    ));
}

#[test]
fn untyped_error_never_cancels_or_restores_a_prompt() {
    let mut request = TurnRetraction::new(
        SessionId::new("session"),
        RunId::new("run"),
        1,
        CommandId::new("command"),
    );
    assert!(
        request
            .apply_response(ResponseBody::Error {
                code: "invalid_argument".into(),
                message: "too_late".into(),
                retryable: false,
                data: None,
            })
            .is_err()
    );
    assert!(matches!(
        request.request_body(),
        RequestBody::TurnRetract { .. }
    ));
}
