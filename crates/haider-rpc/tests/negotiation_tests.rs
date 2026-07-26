#![allow(clippy::expect_used)]

mod common;

use common::capabilities;
use haider_rpc::{Capability, ClientKind, Hello, ServerRange, negotiate};

fn hello(protocol_min: u32, protocol_max: u32) -> Hello {
    Hello {
        protocol_min,
        protocol_max,
        client_name: "haider-tui".into(),
        client_version: "0.0.8".into(),
        client_instance_id: "client-instance-negotiation".into(),
        client_kind: ClientKind::Tui,
        capabilities_requested: capabilities([
            Capability::View,
            Capability::Control,
            Capability::Unknown,
        ]),
        max_receive_frame: 1024 * 1024,
    }
}

#[test]
fn negotiation_selects_implemented_overlap_and_capability_intersection() {
    let negotiated = negotiate(
        &hello(1, 4),
        &ServerRange {
            protocol_min: 1,
            protocol_max: 3,
            capabilities: capabilities([Capability::View]),
        },
    )
    .expect("negotiate");

    assert_eq!(negotiated.protocol, 1);
    assert_eq!(
        negotiated.capabilities_granted,
        capabilities([Capability::View])
    );
}

#[test]
fn negotiation_rejects_ranges_that_only_overlap_at_unimplemented_versions() {
    let error = negotiate(
        &hello(2, 4),
        &ServerRange {
            protocol_min: 2,
            protocol_max: 3,
            capabilities: capabilities([Capability::View]),
        },
    )
    .expect_err("wire v1 is not in the overlap");

    assert_eq!(error.code, "protocol_version_mismatch");
}

#[test]
fn negotiation_never_grants_unrequested_capabilities() {
    let mut client = hello(1, 1);
    client.capabilities_requested = capabilities([Capability::View]);
    let negotiated = negotiate(
        &client,
        &ServerRange {
            protocol_min: 1,
            protocol_max: 1,
            capabilities: capabilities([Capability::View, Capability::Control]),
        },
    )
    .expect("negotiate");

    assert_eq!(
        negotiated.capabilities_granted,
        capabilities([Capability::View])
    );
}

#[test]
fn negotiation_rejects_disjoint_protocol_ranges() {
    let error = negotiate(
        &hello(1, 2),
        &ServerRange {
            protocol_min: 3,
            protocol_max: 4,
            capabilities: capabilities([]),
        },
    )
    .expect_err("disjoint ranges must fail");

    assert_eq!(error.code, "protocol_version_mismatch");
    assert!(error.fatal);
}

#[test]
fn negotiation_never_grants_the_unknown_capability_even_when_both_sides_list_it() {
    // hello() requests Unknown alongside View and Control; Unknown exists
    // only as a decode artifact and must never become a grant.
    let negotiated = negotiate(
        &hello(1, 1),
        &ServerRange {
            protocol_min: 1,
            protocol_max: 1,
            capabilities: capabilities([Capability::View, Capability::Unknown]),
        },
    )
    .expect("negotiate");

    assert_eq!(
        negotiated.capabilities_granted,
        capabilities([Capability::View])
    );
}

#[test]
fn negotiation_rejects_invalid_client_range() {
    let error = negotiate(
        &hello(2, 1),
        &ServerRange {
            protocol_min: 1,
            protocol_max: 1,
            capabilities: capabilities([]),
        },
    )
    .expect_err("client range invalid");

    assert_eq!(error.code, "invalid_client_protocol_range");
}

#[test]
fn negotiation_rejects_invalid_server_range() {
    let error = negotiate(
        &hello(1, 1),
        &ServerRange {
            protocol_min: 2,
            protocol_max: 1,
            capabilities: capabilities([]),
        },
    )
    .expect_err("server range invalid");

    assert_eq!(error.code, "invalid_server_protocol_range");
}
