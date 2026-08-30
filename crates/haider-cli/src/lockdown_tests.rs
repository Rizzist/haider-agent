#![allow(clippy::expect_used)]

use super::{parse, render_status};

#[test]
fn lockdown_cli_parses_status_quota_and_json_without_signed_sizes() {
    assert_eq!(parse(&["status".into()]).expect("status"), (None, false));
    assert_eq!(
        parse(&["quota".into(), "--json".into()]).expect("quota JSON"),
        (None, true)
    );
    assert_eq!(
        parse(&[
            "quota".into(),
            "--set".into(),
            "1073741824".into(),
            "--json".into(),
        ])
        .expect("set quota JSON"),
        (Some(1_073_741_824), true)
    );
    assert!(parse(&["quota".into(), "--set".into(), "-1".into()]).is_err());
}

/// MUTATION CHECK: rename the schema or omit any typed status field.
/// Expected failure: the exact CLI JSON document diverges.
#[test]
fn lockdown_status_json_golden_is_exact() {
    let status = haider_rpc::LockdownStatusWire {
        provider: None,
        activation: None,
        reason: None,
        tools_allowed: vec!["fs_read".into(), "web_search".into()],
        quota_used: 4_096,
        quota_limit: 1_073_741_824,
    };
    assert_eq!(
        render_status(&status, true).expect("JSON status"),
        concat!(
            "{\n",
            "  \"schema\": \"haider.lockdown.v1\",\n",
            "  \"status\": {\n",
            "    \"tools_allowed\": [\n",
            "      \"fs_read\",\n",
            "      \"web_search\"\n",
            "    ],\n",
            "    \"quota_used\": 4096,\n",
            "    \"quota_limit\": 1073741824\n",
            "  }\n",
            "}"
        )
    );
}
