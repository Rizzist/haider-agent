#![allow(clippy::expect_used)]

use super::ssh_profiles::{ssh_list_response, ssh_profiles_available};
use haider_rpc::{
    CapabilitySet, FEATURE_SSH_PROFILES_V1, LifecyclePhase, ResponseBody, SshProfileWire, Welcome,
};

fn welcome() -> Welcome {
    Welcome {
        protocol: 1,
        instance_id: "instance".into(),
        daemon_generation: 1,
        frame_limit: 1_024,
        profile_id: "profile".into(),
        daemon_version: "test".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::default(),
        features: Default::default(),
        user_command_withheld: false,
        encoding: None,
    }
}

#[test]
fn ssh_profiles_surface_obeys_feature_absence_law() {
    let mut welcome = welcome();
    assert!(!ssh_profiles_available(&welcome));
    welcome.features.insert(FEATURE_SSH_PROFILES_V1.into());
    assert!(ssh_profiles_available(&welcome));
}

#[test]
fn public_list_response_cannot_carry_auth_material() {
    let profile = SshProfileWire {
        name: "prod".into(),
        description: Some("Production".into()),
        host: "prod.example.invalid".into(),
        port: 22,
        user: "deploy".into(),
        default_cwd: None,
        host_key: None,
        last_used_ms: None,
        multiplexing: true,
        in_scope: true,
    };
    let profiles = ssh_list_response(ResponseBody::SshList {
        profiles: vec![profile.clone()],
    })
    .expect("list response");
    assert_eq!(profiles, [profile]);
    let encoded = serde_json::to_string(&profiles).expect("encode public profiles");
    assert!(!encoded.contains("vault"));
    assert!(!encoded.contains("secret"));
    assert!(!encoded.contains("password\":"));
}
