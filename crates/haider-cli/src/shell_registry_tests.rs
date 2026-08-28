#![allow(clippy::expect_used)]

use super::*;

#[test]
fn shell_list_json_shape_is_stable_and_public() {
    let shells = [haider_rpc::ShellWire {
        id: "sh-0123456789abcdef0123".into(),
        kind: haider_rpc::ShellKindWire::Ssh {
            profile: "prod".into(),
        },
        status: haider_rpc::ShellStatusWire::Running,
        title: "release checks".into(),
        cwd_or_host: "prod.example.invalid".into(),
        created_at_ms: 1,
        last_activity_ms: 2,
        bytes_out: 3,
    }];
    let document = serde_json::to_value(ShellListDocument {
        schema: SHELL_LIST_SCHEMA,
        shells: &shells,
    })
    .expect("shell list JSON");
    assert_eq!(
        document,
        serde_json::json!({
            "schema": "haider.shell.list.v1",
            "shells": [{
                "id": "sh-0123456789abcdef0123",
                "kind": {"kind": "ssh", "profile": "prod"},
                "status": {"status": "running"},
                "title": "release checks",
                "cwd_or_host": "prod.example.invalid",
                "created_at_ms": 1,
                "last_activity_ms": 2,
                "bytes_out": 3
            }]
        })
    );
}
