#![allow(clippy::expect_used)]

use super::lockdown::{
    LockdownClientError, lockdown_set_quota_response, lockdown_status_response,
    provider_lockdown_available,
};
use haider_rpc::{
    CapabilitySet, FEATURE_PROVIDER_LOCKDOWN_V1, LifecyclePhase, LockdownStatusWire, ResponseBody,
    Welcome,
};

#[test]
fn feature_absence_makes_lockdown_helpers_absent() {
    let mut welcome = Welcome {
        protocol: 1,
        instance_id: "instance".into(),
        daemon_generation: 1,
        frame_limit: 1024,
        profile_id: "profile".into(),
        daemon_version: "test".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::default(),
        features: Default::default(),
        user_command_withheld: false,
        encoding: None,
    };
    assert!(!provider_lockdown_available(&welcome));
    welcome
        .features
        .insert(FEATURE_PROVIDER_LOCKDOWN_V1.to_owned());
    assert!(provider_lockdown_available(&welcome));
}

#[test]
fn typed_status_and_quota_responses_do_not_parse_prose() {
    let status = LockdownStatusWire {
        provider: Some("research".into()),
        activation: None,
        reason: None,
        tools_allowed: vec!["fs_read".into()],
        quota_used: 8,
        quota_limit: 64,
    };
    assert_eq!(
        lockdown_status_response(ResponseBody::LockdownStatus {
            status: status.clone(),
        })
        .expect("status response"),
        status
    );
    assert_eq!(
        lockdown_set_quota_response(ResponseBody::LockdownSetQuota {
            status: status.clone(),
        })
        .expect("quota response"),
        status
    );
    assert!(matches!(
        lockdown_status_response(ResponseBody::DaemonShutdown {}),
        Err(LockdownClientError::UnexpectedBody)
    ));
}
