//! Connection compatibility and bounded cursor recovery are separate facts.

use haider_protocol::envelope::RawPayload;
use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope};

/// The journal is the lag buffer: one fresh subscription repairs delivery
/// loss; one retry lets a transient replay failure recover. A second failed
/// replay at the SAME applied cursor exhausts that recovery budget.
/// Count completed failed attempts, never event bursts
/// or wall time (#94).
pub const RESYNC_ATTEMPTS: u8 = 2;

#[derive(Debug)]
pub(crate) struct Resync {
    pub after_seq: u64,
    pub attempts: u8,
    pub blocked: bool,
    pub cause: String,
}

/// Only the discriminator is diagnostic data; never include payload contents.
pub(crate) fn payload_type(payload: &RawPayload) -> String {
    payload
        .type_tag()
        .filter(|tag| !tag.trim().is_empty())
        .map_or_else(
            || "<missing/empty/non-string>".into(),
            |tag| tag.chars().filter(|c| !c.is_control()).take(80).collect(),
        )
}

pub(crate) fn compatibility(
    client: &str,
    daemon: Option<&str>,
    protocol: Option<u32>,
    observed: &str,
) -> Option<ErrorPresentation> {
    if client.trim().is_empty() {
        return None;
    }
    let daemon = daemon.filter(|version| !version.trim().is_empty())?;
    let protocol = protocol?;
    // daemon_generation is a process incarnation, not a protocol generation.
    // Never compare it to a client or worker generation.
    if client == daemon {
        return None;
    }
    Some(ErrorPresentation::new(
        "client-daemon-incompatible",
        "Client/daemon version mismatch — reconnect",
        format!(
            "client {client} (protocol {}), daemon {daemon} (protocol {protocol}); {observed}; reconnect with /reconnect",
            haider_rpc::WIRE_PROTOCOL_VERSION,
        ),
        ErrorScope::Session,
        [ErrorAction::Reconnect],
    ))
}

pub(crate) fn failed(after_seq: u64, reason: &str) -> ErrorPresentation {
    ErrorPresentation::new(
        "session-resync-failed",
        "Session resync failed — reconnect",
        format!(
            "{RESYNC_ATTEMPTS} journal replays made no progress after {after_seq}; {reason}; reconnect with /reconnect",
        ),
        ErrorScope::Session,
        [ErrorAction::Reconnect],
    )
}
