//! Version and capability negotiation.
//!
//! These are handshake helper types, not wire shapes: nothing in this module
//! is serialized. The daemon publishes a negotiation outcome on the wire
//! through [`crate::Welcome`].

use crate::{Capability, CapabilitySet, Hello, ProtocolError, WIRE_PROTOCOL_VERSION};

/// Server-side input to [`negotiate`]: the protocol range the daemon
/// implements (inclusive) and the capabilities it is willing to grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRange {
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub capabilities: CapabilitySet,
    /// Whether the server advertises and serves `wire_msgpack_v1`.
    pub supports_msgpack: bool,
}

/// Successful handshake selection returned by [`negotiate`].
///
/// Not a wire shape: the daemon copies these values into [`crate::Welcome`]
/// (`protocol`, `capabilities_granted`) to publish them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub protocol: u32,
    pub capabilities_granted: CapabilitySet,
    /// Selected post-handshake encoding. `None` means JSON.
    pub encoding: Option<String>,
}

/// Selects the highest mutually supported protocol implemented by this wire
/// crate and the requested, server-supported capability intersection.
///
/// Laws:
/// - The selected protocol is clamped to [`WIRE_PROTOCOL_VERSION`]: an
///   overlap that exists only at versions this crate does not implement is a
///   mismatch, not a silent downgrade.
/// - Capabilities are granted by intersection only — never a capability the
///   client did not request, and never [`Capability::Unknown`], which exists
///   purely as a decode artifact.
/// - Every error is a fatal [`ProtocolError`] ready to be sent before close,
///   with one of the codes `invalid_client_protocol_range`,
///   `invalid_server_protocol_range`, or `protocol_version_mismatch`.
pub fn negotiate(client: &Hello, server_range: &ServerRange) -> Result<Negotiated, ProtocolError> {
    if client.protocol_min > client.protocol_max {
        return Err(protocol_error(
            "invalid_client_protocol_range",
            format!(
                "client protocol minimum {} exceeds maximum {}",
                client.protocol_min, client.protocol_max
            ),
        ));
    }
    if server_range.protocol_min > server_range.protocol_max {
        return Err(protocol_error(
            "invalid_server_protocol_range",
            format!(
                "server protocol minimum {} exceeds maximum {}",
                server_range.protocol_min, server_range.protocol_max
            ),
        ));
    }

    let overlap_min = client
        .protocol_min
        .max(server_range.protocol_min)
        .max(WIRE_PROTOCOL_VERSION);
    let overlap_max = client
        .protocol_max
        .min(server_range.protocol_max)
        .min(WIRE_PROTOCOL_VERSION);
    if overlap_min > overlap_max {
        return Err(protocol_error(
            "protocol_version_mismatch",
            format!(
                "client range {}..={} does not overlap server range {}..={}",
                client.protocol_min,
                client.protocol_max,
                server_range.protocol_min,
                server_range.protocol_max
            ),
        ));
    }

    let capabilities_granted = client
        .capabilities_requested
        .intersection(&server_range.capabilities)
        .copied()
        .filter(|capability| *capability != Capability::Unknown)
        .collect();

    Ok(Negotiated {
        protocol: overlap_max,
        capabilities_granted,
        encoding: client
            .encodings
            .iter()
            .any(|encoding| server_range.supports_msgpack && encoding == "msgpack")
            .then(|| "msgpack".to_owned()),
    })
}

fn protocol_error(code: &str, message: String) -> ProtocolError {
    ProtocolError {
        code: code.to_owned(),
        message,
        fatal: true,
        presentation: None,
        failed_write_ids: Vec::new(),
    }
}
