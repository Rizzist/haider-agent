#![allow(clippy::expect_used)]

use super::{is_next_sequence, known_omitted_children, negotiated_child_limit};

/// MUTATION CHECK: accepting either side of the exact next sequence
/// silently advances across a hole or replays a duplicate.
#[test]
fn descendant_gap_detection_accepts_only_the_exact_next_sequence() {
    assert!(is_next_sequence(7, Some(8)));
    assert!(!is_next_sequence(7, Some(7)), "duplicate is not progress");
    assert!(!is_next_sequence(7, Some(9)), "gap is not progress");
    assert!(!is_next_sequence(7, None), "missing page is not progress");
}

#[test]
fn descendant_fanout_negotiation_clamps_to_the_typed_hard_limit() {
    assert_eq!(negotiated_child_limit(1), 1);
    assert_eq!(negotiated_child_limit(u32::MAX), 64);
}

/// A cursor-seeded child beyond the bounded scan may itself be the scan's
/// truncation witness, so the incomplete count must remain a lower bound.
#[test]
fn descendant_incomplete_truncation_never_overstates_omitted_children() {
    assert_eq!(known_omitted_children(512, 0, 1, true), 512);
    assert_eq!(known_omitted_children(512, 1, 1, true), 512);
}
