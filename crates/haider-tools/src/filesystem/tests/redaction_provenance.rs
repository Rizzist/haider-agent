use super::*;

#[test]
fn redacted_freshness_is_keyed_and_rechecks_exact_original_bytes() {
    let original = b"password=violet-sunrise\n";
    let keyed = private_freshness_digest(original, "synthetic-session").expect("OS random key");
    assert!(keyed.starts_with("blake3k:synthetic-session:"));
    assert!(!keyed.contains(&mutation_digest(original)));
    assert_eq!(freshness_digest_for_expected(original, Some(&keyed)), keyed);
    assert_ne!(
        freshness_digest_for_expected(b"password=violet-morning\n", Some(&keyed)),
        keyed
    );
    assert_eq!(
        freshness_digest_for_expected(original, None),
        mutation_digest(original)
    );
}
