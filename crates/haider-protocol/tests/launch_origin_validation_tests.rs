use haider_protocol::session::{LaunchOriginPathKindV1, LaunchOriginPathV1};

#[test]
fn ingress_rejects_raw_arabic_letter_mark() {
    let path = LaunchOriginPathV1 {
        kind: LaunchOriginPathKindV1::Absolute,
        display: Some("/opt/a\u{061c}b".into()),
    };

    assert!(path.validate().is_err());
}
