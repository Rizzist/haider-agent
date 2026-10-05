use super::*;

#[tokio::test(start_paused = true)]
async fn request_upload_does_not_consume_idle_budget() {
    let idle = ProviderIdleDeadline::default();
    idle.begin_attempt(Some(Duration::from_secs(30)));
    idle.pause_for_upload();
    tokio::time::advance(Duration::from_secs(45)).await;
    assert!(idle.expired().is_none());

    idle.resume_after_upload();
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(idle.expired().is_none());
    tokio::time::advance(Duration::from_secs(1)).await;
    let error = match idle.expired() {
        Some(error) => error,
        None => panic!("active idle budget did not expire"),
    };
    let evidence = match error.idle_timeout {
        Some(evidence) => evidence,
        None => panic!("idle timeout omitted typed evidence"),
    };
    assert_eq!(evidence.idle_elapsed_ms, 30_000);
    assert_eq!(evidence.elapsed_ms, 30_000);
}

#[tokio::test(start_paused = true)]
async fn terminal_idle_error_names_the_last_response_open_cause() {
    let idle = ProviderIdleDeadline::default();
    idle.begin_attempt(Some(Duration::from_secs(1)));
    idle.record_error(&ProviderError::new(
        ProviderErrorKind::Transport,
        "Anthropic response did not open within the configured response-open budget after request upload completed; budget_ms=60000",
    ));
    tokio::time::advance(Duration::from_secs(1)).await;

    let error = match idle.expired() {
        Some(error) => error,
        None => panic!("idle timeout did not expire"),
    };
    assert!(error.message.contains("Anthropic response did not open"));
    assert!(
        error
            .presentation
            .detail
            .contains("Anthropic response did not open")
    );
}
