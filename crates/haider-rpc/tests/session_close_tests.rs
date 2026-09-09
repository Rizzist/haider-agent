#![allow(clippy::expect_used)]

use haider_rpc::{AttachmentId, FEATURE_SESSION_CLOSE_V1, RequestBody, ResponseBody};

#[test]
fn close_is_an_additive_detach_shape_with_an_explicit_completion_witness() {
    let legacy = serde_json::json!({"method":"session.detach","attachment_id":"a"});
    let request: RequestBody = serde_json::from_value(legacy.clone()).expect("old request");
    assert_eq!(request.additive_shape_feature(), None);
    assert_eq!(serde_json::to_value(request).expect("encode"), legacy);
    let close = RequestBody::SessionDetach {
        attachment_id: AttachmentId::new("a"),
        close_session: true,
    };
    assert_eq!(
        close.additive_shape_feature(),
        Some(FEATURE_SESSION_CLOSE_V1)
    );
    let encoded = serde_json::to_value(&close).expect("encode close");
    assert_eq!(encoded["method"], "session.detach");
    assert_eq!(encoded["close_session"], true);
    assert_eq!(
        serde_json::from_value::<RequestBody>(encoded).expect("decode close"),
        close
    );
    let response: ResponseBody = serde_json::from_value(legacy.clone()).expect("old response");
    assert!(matches!(
        &response,
        ResponseBody::SessionDetach {
            closed_session_id: None,
            ..
        }
    ));
    assert_eq!(
        serde_json::to_value(response).expect("encode response"),
        legacy
    );
}
