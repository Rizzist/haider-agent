//! Stable client/daemon trace correlation helpers.

use crate::ids::SessionId;

/// Legacy numeric correlation retained for the client-side terminal trace.
/// Provider and daemon request-attempt records use the declared session/run/
/// turn/request coordinates instead; this digest is not a transport identity.
#[doc(hidden)]
#[must_use]
pub fn turn_trace_ordinal(session_id: &SessionId, accepted_seq: u64) -> u64 {
    let mut digest = 0xcbf2_9ce4_8422_2325_u64;
    for byte in session_id
        .as_str()
        .as_bytes()
        .iter()
        .copied()
        .chain(accepted_seq.to_le_bytes())
    {
        digest ^= u64::from(byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
    digest.max(1)
}
