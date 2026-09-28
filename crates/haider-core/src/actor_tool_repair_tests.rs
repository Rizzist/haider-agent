use super::actor_tool_repair::{
    advance_output_truncation_count, repeated_output_truncation_on_next,
};

#[test]
fn only_invalid_tool_calls_consume_the_malformed_strike() {
    let truncation: haider_protocol::tool::BoundedResult = serde_json::from_value(
        serde_json::json!({"preview":"truncated","truncated":false,"data":{
            "kind":"output_limit_truncation","tool":"fs_write","message":"partial"
        }}),
    )
    .unwrap_or_else(|error| panic!("truncation result: {error}"));
    assert!(!super::consumes_malformed_tool_strike(&truncation));
    let malformed: haider_protocol::tool::BoundedResult = serde_json::from_value(
        serde_json::json!({"preview":"malformed","truncated":false,"data":{
            "kind":"invalid_tool_call","tool":"fs_write","message":"invalid JSON"
        }}),
    )
    .unwrap_or_else(|error| panic!("malformed result: {error}"));
    assert!(super::consumes_malformed_tool_strike(&malformed));
}

#[test]
fn three_consecutive_output_truncations_return_the_repeated_tool_error() {
    let mut count = 0;
    assert!(!repeated_output_truncation_on_next(count));
    count = advance_output_truncation_count(count);
    assert!(!repeated_output_truncation_on_next(count));
    count = advance_output_truncation_count(count);
    assert!(repeated_output_truncation_on_next(count));
    count = advance_output_truncation_count(count);
    assert_eq!(count, 3);
    assert_eq!(advance_output_truncation_count(count), 3);
}
