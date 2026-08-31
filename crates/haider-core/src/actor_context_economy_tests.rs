#![allow(clippy::expect_used)]

use super::{
    Block, ContextCompactionTier, HarnessConfig, ImageBlockRef, Message, context_accounting,
    estimate_provider_request_input_tokens, trim_stale_tool_pairs,
};
use haider_protocol::ids::{ArtifactRef, DeviceId, SessionId};

fn tool_call(call_id: &str) -> Message {
    Message::assistant(vec![Block::ToolCall {
        call_id: call_id.to_owned(),
        name: "read_file".into(),
        args: serde_json::json!({"path": format!("/{call_id}")}),
    }])
}

fn tool_result(call_id: &str, images: Vec<ImageBlockRef>) -> Message {
    Message::tool_result_with_images(
        call_id,
        format!("complete stale output for {call_id}"),
        false,
        images,
    )
}

#[test]
fn structural_trim_drops_only_old_complete_pairs_and_preserves_other_blocks() {
    let mut messages = Vec::new();
    let mut original_non_tool_blocks = Vec::new();
    for ordinal in 0..30 {
        let call_id = format!("call-{ordinal:02}");
        if ordinal == 0 {
            let prose = Block::Text {
                text: "analysis beside the oldest call stays byte-identical".into(),
            };
            original_non_tool_blocks.push(prose.clone());
            messages.push(Message::assistant(vec![
                prose,
                Block::ToolCall {
                    call_id: call_id.clone(),
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "/oldest"}),
                },
            ]));
        } else {
            messages.push(tool_call(&call_id));
        }
        messages.push(tool_result(&call_id, Vec::new()));
    }
    messages.push(Message::assistant(vec![Block::ToolCall {
        call_id: "orphan-call".into(),
        name: "never_settled".into(),
        args: serde_json::json!({}),
    }]));
    let current_turn_start = messages.len();
    messages.push(Message::user_text("current user turn stays verbatim"));
    messages.push(tool_call("current-call"));
    messages.push(tool_result("current-call", Vec::new()));

    let outcome = trim_stale_tool_pairs(&mut messages, current_turn_start, 24);

    assert_eq!(outcome.removed_pairs, 6);
    assert_eq!(
        outcome.removed_tool_call_ids,
        (0..6)
            .map(|ordinal| format!("call-{ordinal:02}"))
            .collect::<Vec<_>>()
    );
    let remaining_non_tool_blocks = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| !matches!(block, Block::ToolCall { .. } | Block::ToolResult { .. }))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        remaining_non_tool_blocks,
        [
            original_non_tool_blocks,
            vec![Block::Text {
                text: "current user turn stays verbatim".into(),
            }]
        ]
        .concat()
    );
    assert!(
        messages.iter().flat_map(|message| &message.blocks).any(
            |block| matches!(block, Block::ToolCall { call_id, .. } if call_id == "orphan-call")
        )
    );
    assert_eq!(
        messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter(|block| matches!(block,
                Block::ToolCall { call_id, .. } | Block::ToolResult { call_id, .. }
                if call_id == "current-call"
            ))
            .count(),
        2,
        "current-turn pairs are outside the structural trim domain"
    );
    for ordinal in 6..30 {
        let call_id = format!("call-{ordinal:02}");
        let blocks = messages.iter().flat_map(|message| &message.blocks);
        assert_eq!(
            blocks
                .filter(|block| matches!(block,
                    Block::ToolCall { call_id: found, .. }
                    | Block::ToolResult { call_id: found, .. }
                    if found == &call_id
                ))
                .count(),
            2,
            "newest retained pair {call_id} stays complete"
        );
    }
}

#[test]
fn structural_trim_keeps_an_image_bearing_exchange_whole() {
    let image = ImageBlockRef {
        artifact: ArtifactRef::new("image-artifact"),
        media_type: "image/png".into(),
        width: 16,
        height: 16,
        byte_len: 32,
    };
    let mut messages = vec![
        tool_call("image-call"),
        tool_result("image-call", vec![image]),
        tool_call("discarded-call"),
        tool_result("discarded-call", Vec::new()),
        tool_call("newest-call"),
        tool_result("newest-call", Vec::new()),
        Message::user_text("current"),
    ];

    let outcome = trim_stale_tool_pairs(&mut messages, 6, 1);

    assert_eq!(outcome.removed_tool_call_ids, ["discarded-call"]);
    for call_id in ["image-call", "newest-call"] {
        assert_eq!(
            messages
                .iter()
                .flat_map(|message| &message.blocks)
                .filter(|block| matches!(block,
                    Block::ToolCall { call_id: found, .. }
                    | Block::ToolResult { call_id: found, .. }
                    if found == call_id
                ))
                .count(),
            2
        );
    }
}

#[test]
fn context_accounting_reports_headroom_and_the_next_mode_specific_tier() {
    let mut fast = HarnessConfig::for_session(
        SessionId::new("context-accounting-fast"),
        DeviceId::new("context-accounting-device"),
        1,
        1,
    );
    fast.context_window = Some(100_000);
    fast.reserved_output_tokens = 10_000;
    fast.structural_context_trimming = true;
    let accounting = context_accounting(&fast, 60_000, 100_000);
    assert_eq!(accounting.used_tokens, 60_000);
    assert_eq!(accounting.model_limit_tokens, 100_000);
    assert_eq!(accounting.remaining_tokens, 40_000);
    assert_eq!(accounting.usage_basis_points, 6_000);
    assert_eq!(
        accounting.next_tier,
        Some(ContextCompactionTier::StructuralTrim12)
    );
    assert_eq!(accounting.next_tier_at_tokens, Some(75_000));
    assert_eq!(accounting.tokens_until_next_tier, Some(15_000));

    fast.structural_context_trimming = false;
    let default = context_accounting(&fast, 60_000, 100_000);
    assert_eq!(default.next_tier, Some(ContextCompactionTier::Summarize));
    assert_eq!(default.next_tier_at_tokens, Some(85_000));
    assert_eq!(default.tokens_until_next_tier, Some(25_000));
}

#[test]
fn structural_measurement_reports_comparable_before_and_after_estimates() {
    let mut messages = Vec::new();
    for ordinal in 0..40 {
        let call_id = format!("measurement-call-{ordinal:02}");
        messages.push(tool_call(&call_id));
        messages.push(Message::tool_result(
            &call_id,
            format!("measurement-output-{ordinal:02}-{}", "z".repeat(4_096)),
            false,
        ));
    }
    messages.push(Message::user_text("measurement current turn"));
    let protected_start = messages.len().saturating_sub(1);
    let before = estimate_provider_request_input_tokens(&messages, &None, &[], &[]);
    let outcome = trim_stale_tool_pairs(&mut messages, protected_start, 24);
    let after = estimate_provider_request_input_tokens(&messages, &None, &[], &[]);

    assert_eq!(outcome.removed_pairs, 16);
    assert!(after < before);
    eprintln!(
        "structural_measurement before_estimated_tokens={before} after_estimated_tokens={after} saved_estimated_tokens={} saved_percent_basis_points={}",
        before.saturating_sub(after),
        u128::from(before.saturating_sub(after)).saturating_mul(10_000) / u128::from(before),
    );
}

#[test]
fn structural_benchmark_reports_cumulative_estimated_savings_per_million() {
    let mut messages = Vec::new();
    for ordinal in 0..120 {
        let call_id = format!("million-call-{ordinal:03}");
        messages.push(tool_call(&call_id));
        messages.push(Message::tool_result(
            &call_id,
            format!("million-output-{ordinal:03}-{}", "m".repeat(32_768)),
            false,
        ));
    }
    messages.push(Message::user_text("million-fixture current turn"));
    let protected_start = messages.len().saturating_sub(1);
    let before = estimate_provider_request_input_tokens(&messages, &None, &[], &[]);
    let tier_one = trim_stale_tool_pairs(&mut messages, protected_start, 24);
    let after_tier_one = estimate_provider_request_input_tokens(&messages, &None, &[], &[]);
    let tier_two = trim_stale_tool_pairs(&mut messages, protected_start, 12);
    let after_tier_two = estimate_provider_request_input_tokens(&messages, &None, &[], &[]);
    let (economy, _) = super::ContextEconomy::default().record(
        ContextCompactionTier::StructuralTrim24,
        before,
        after_tier_one,
    );
    let (economy, _) = economy.record(
        ContextCompactionTier::StructuralTrim12,
        after_tier_one,
        after_tier_two,
    );
    let saved_per_million = u128::from(economy.cumulative_estimated_tokens_saved)
        .saturating_mul(1_000_000)
        / u128::from(before.max(1));

    assert_eq!(tier_one.removed_pairs, 96);
    assert_eq!(tier_two.removed_pairs, 12);
    assert_eq!(
        economy.cumulative_estimated_tokens_saved,
        before.saturating_sub(after_tier_two)
    );
    assert!(before >= 980_000);
    assert!(after_tier_two < after_tier_one && after_tier_one < before);
    eprintln!(
        "context_economy_benchmark before_estimated_tokens={before} after_tier_one_estimated_tokens={after_tier_one} after_tier_two_estimated_tokens={after_tier_two} cumulative_estimated_tokens_saved={} estimated_tokens_saved_per_million={saved_per_million}",
        economy.cumulative_estimated_tokens_saved,
    );
}
