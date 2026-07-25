//! Smoke test establishing the tests-live-in-tests/ convention for this crate.

#[test]
fn crate_name_is_stable() {
    assert_eq!(haider_protocol::CRATE_NAME, "haider-protocol");
}
