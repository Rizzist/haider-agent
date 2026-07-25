//! Smoke test establishing the tests-live-in-tests/ convention for this crate.

#[test]
fn crate_name_is_stable() {
    assert_eq!(haider_provider::CRATE_NAME, "haider-provider");
}
