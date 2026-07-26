//! Version and capability negotiation.

use crate::{Capability, Hello, Negotiated, ProtocolError, ServerRange, WIRE_PROTOCOL_VERSION};

/// Selects the highest mutually supported protocol implemented by this wire
/// crate and the requested, server-supported capability intersection.
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
    })
}

fn protocol_error(code: &str, message: String) -> ProtocolError {
    ProtocolError {
        code: code.to_owned(),
        message,
        fatal: true,
    }
}
