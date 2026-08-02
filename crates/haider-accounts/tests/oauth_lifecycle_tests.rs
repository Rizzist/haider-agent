#![allow(clippy::expect_used)]

use haider_accounts::{OAuthIdentityV1, OAuthTokenBundleV1};
use zeroize::Zeroizing;

fn bundle() -> OAuthTokenBundleV1 {
    OAuthTokenBundleV1::new(
        "kimi-oauth".into(),
        "https://auth.kimi.com".into(),
        "public-client".into(),
        None,
        "Bearer".into(),
        Zeroizing::new(b"access-lifecycle-sentinel".to_vec()),
        Some(Zeroizing::new(b"refresh-lifecycle-sentinel".to_vec())),
        10_000,
        Some(20_000),
        Vec::new(),
        OAuthIdentityV1 {
            subject_hash: "subject-hash".into(),
            display_identity: "Kimi Code subscription".into(),
        },
        7,
    )
    .expect("bundle")
}

/// MUTATION CHECK: always emit the B6k lifecycle trailer, reject a legacy
/// trailer-free bundle, fail to round-trip either persisted deadline, or
/// expose/drop the optional import-source fingerprint used to prevent a stale
/// source from rolling back a rotated refresh token.
/// Expected RUNTIME failure: the legacy bytes gain a suffix, decoding fails,
/// or the exact lifecycle/fingerprint values differ.
#[test]
fn lifecycle_extension_is_trailing_optional_and_legacy_compatible() {
    let legacy = bundle().encode().expect("legacy-compatible encode");
    let decoded = OAuthTokenBundleV1::decode(&legacy).expect("legacy-compatible decode");
    assert_eq!(decoded.refresh_after_unix_ms, None);
    assert_eq!(decoded.refresh_rejected_until_unix_ms, None);

    let extended = bundle()
        .with_refresh_after(8_000)
        .with_refresh_rejected_until(9_000)
        .encode()
        .expect("lifecycle encode");
    assert!(
        extended.starts_with(&legacy),
        "B6k lifecycle state is an optional trailing extension"
    );
    let decoded = OAuthTokenBundleV1::decode(&extended).expect("lifecycle decode");
    assert_eq!(decoded.refresh_after_unix_ms, Some(8_000));
    assert_eq!(decoded.refresh_rejected_until_unix_ms, Some(9_000));

    let fingerprint = [0x5a; 32];
    let imported = bundle()
        .with_import_source_access_fingerprint(fingerprint)
        .encode()
        .expect("import provenance encode");
    assert!(
        imported.starts_with(&legacy),
        "import provenance is an optional trailing extension"
    );
    let decoded = OAuthTokenBundleV1::decode(&imported).expect("import provenance decode");
    assert_eq!(
        decoded.import_source_access_fingerprint(),
        Some(fingerprint)
    );
}
