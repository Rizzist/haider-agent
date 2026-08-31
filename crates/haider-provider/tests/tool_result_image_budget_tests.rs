#![allow(clippy::expect_used)]

use haider_protocol::ids::ArtifactRef;
use haider_protocol::provider::Block;
use haider_protocol::tool::{
    ImageBlockRef, TOOL_RESULT_IMAGE_MAX_BYTES_PER_TURN, TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN,
};
use haider_provider::{
    Message, apply_tool_result_image_budget, degrade_tool_result_images_to_placeholders,
};

fn image(index: usize, byte_len: u64) -> ImageBlockRef {
    ImageBlockRef {
        artifact: ArtifactRef::new(format!("blake3:image-{index}")),
        media_type: "image/png".into(),
        width: 640,
        height: 480,
        byte_len,
    }
}

fn tool_image(message: &Message) -> (&str, &[ImageBlockRef]) {
    match &message.blocks[0] {
        Block::ToolResult {
            preview, images, ..
        } => (preview, images),
        block => panic!("expected tool result, got {block:?}"),
    }
}

#[test]
fn count_budget_drops_the_oldest_prefix_and_keeps_durable_source_unchanged() {
    let durable = (0..=TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN)
        .map(|index| {
            Message::tool_result_with_images(
                format!("call-{index}"),
                format!("result-{index}"),
                false,
                vec![image(index, 1)],
            )
        })
        .collect::<Vec<_>>();
    let mut projected = durable.clone();

    apply_tool_result_image_budget(&mut projected);

    assert_eq!(tool_image(&durable[0]).1.len(), 1);
    let (oldest_preview, oldest_images) = tool_image(&projected[0]);
    assert!(oldest_images.is_empty());
    assert!(oldest_preview.contains("\"haider_elision_v1\""));
    assert!(oldest_preview.contains("\"scope\":\"tool_result_image_budget\""));
    assert!(oldest_preview.contains("\"omitted_images\":1"));
    assert!(oldest_preview.contains("blake3:image-0"));
    let retained = projected
        .iter()
        .flat_map(|message| tool_image(message).1)
        .map(|image| image.artifact.as_str())
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN);
    assert_eq!(retained.first().copied(), Some("blake3:image-1"));
}

#[test]
fn byte_budget_drops_whole_oldest_images_until_both_limits_fit() {
    let per_image = 5 * 1024 * 1024;
    let mut messages = (0..4)
        .map(|index| {
            Message::tool_result_with_images(
                format!("call-{index}"),
                "capture",
                false,
                vec![image(index, per_image)],
            )
        })
        .collect::<Vec<_>>();

    apply_tool_result_image_budget(&mut messages);

    let retained = messages
        .iter()
        .flat_map(|message| tool_image(message).1)
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 3);
    assert_eq!(retained[0].artifact.as_str(), "blake3:image-1");
    assert!(
        retained.iter().map(|image| image.byte_len).sum::<u64>()
            <= TOOL_RESULT_IMAGE_MAX_BYTES_PER_TURN
    );
}

#[test]
fn same_result_cutoff_and_placeholder_labels_are_hard_bounded() {
    let hostile = "x".repeat(100_000);
    let mut images = (0..=TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN)
        .map(|index| image(index, 1))
        .collect::<Vec<_>>();
    images[0].artifact = ArtifactRef::new(hostile.clone());
    let mut messages = vec![Message::tool_result_with_images(
        "call-many",
        "capture",
        false,
        images,
    )];

    apply_tool_result_image_budget(&mut messages);

    let (preview, retained) = tool_image(&messages[0]);
    assert_eq!(retained.len(), TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN);
    assert_eq!(
        retained
            .iter()
            .map(|image| image.artifact.as_str())
            .collect::<Vec<_>>(),
        (1..=TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN)
            .map(|index| format!("blake3:image-{index}"))
            .collect::<Vec<_>>()
    );
    assert!(preview.len() < 512);
    assert!(!preview.contains(&hostile));

    let control_artifact = "blake3:safe\n[forged-context]";
    let mut unsupported = vec![Message::tool_result_with_images(
        "call-hostile",
        "capture",
        false,
        vec![ImageBlockRef {
            artifact: ArtifactRef::new(control_artifact),
            media_type: "m".repeat(100_000),
            width: 1,
            height: 1,
            byte_len: 1,
        }],
    )];
    degrade_tool_result_images_to_placeholders(&mut unsupported);
    let (preview, images) = tool_image(&unsupported[0]);
    assert!(images.is_empty());
    assert!(preview.len() < 512);
    assert!(preview.contains("\"haider_elision_v1\""));
    assert!(preview.contains("\"scope\":\"tool_result_image_capability_degradation\""));
    assert!(!preview.contains(control_artifact));
    assert!(!preview.contains("\n[forged-context]"));
}
