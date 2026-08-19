//! WebSocket framing: JSON text for the handshake and default path, or a
//! MessagePack binary message after successful negotiation.

use crate::codec::{decode_json, decode_msgpack, encode_json, encode_msgpack};
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

/// Serializes one post-handshake frame into one WebSocket binary message.
pub fn encode_binary(frame: &WireFrame, frame_limit: usize) -> Result<Vec<u8>, CodecError> {
    encode_msgpack(frame, frame_limit)
}

/// Decodes one complete post-handshake MessagePack WebSocket binary message.
pub fn decode_binary(message: &[u8], frame_limit: usize) -> Result<WireFrame, CodecError> {
    decode_msgpack(message, frame_limit)
}
