/// MUTATION CHECK: these literals are the additive discovery contract for
/// durable cross-provider fallback and ineffective-compaction promotion.
#[test]
fn resilience_feature_literals_are_pinned() {
    assert_eq!(haider_rpc::FEATURE_FALLBACK_CHAIN_V1, "fallback_chain_v1");
    assert_eq!(
        haider_rpc::FEATURE_COMPACTION_GUARD_V1,
        "compaction_guard_v1"
    );
}
