//! WebSocket framing: one JSON object per UTF-8 text message.

use crate::codec::{decode_json, encode_json};
use crate::{CodecError, WireFrame};

/// Serializes one frame into one WebSocket text message.
///
/// Serialization writes through a bounded sink, so output cannot grow beyond
/// `frame_limit` before the limit error is reported.
pub fn encode(frame: &WireFrame, frame_limit: usize) -> Result<String, CodecError> {
    let bytes = encode_json(frame, frame_limit)?;
    String::from_utf8(bytes).map_err(|error| CodecError::InvalidUtf8(error.utf8_error()))
}

/// Decodes one complete WebSocket text message.
///
/// The byte limit is checked before JSON deserialization.
pub fn decode(message: &str, frame_limit: usize) -> Result<WireFrame, CodecError> {
    decode_json(message.as_bytes(), frame_limit)
}
