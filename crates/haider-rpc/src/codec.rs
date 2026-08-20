//! Shared JSON and MessagePack codec support.

use std::io::Write;

use serde::Serialize;

use crate::WireFrame;

/// Encoding used for post-handshake wire frames.
///
/// Hello and Welcome are always exchanged with the legacy JSON codec. A
/// connection may switch to [`Self::MessagePack`] only after Welcome confirms
/// the negotiation outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WireEncoding {
    /// UTF-8 JSON, the backwards-compatible default.
    #[default]
    Json,
    /// MessagePack binary encoding.
    MessagePack,
}

/// A typed wire-codec failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum CodecError {
    /// The encoded or announced body exceeds the configured limit.
    ///
    /// `announced_len` is `Some` when the offending length was known up front
    /// (a decode input or a UDS length prefix) and `None` when streaming
    /// serialization crossed the limit mid-encode, before the full length
    /// existed anywhere.
    FrameLimitExceeded {
        frame_limit: usize,
        announced_len: Option<usize>,
    },
    /// A zero-length body is invalid for either encoding on both transports.
    EmptyFrame,
    /// A complete UDS body was not UTF-8.
    InvalidUtf8(std::str::Utf8Error),
    /// JSON serialization or decoding failed.
    Json(serde_json::Error),
    /// MessagePack serialization failed.
    MessagePackEncode(rmp_serde::encode::Error),
    /// MessagePack decoding failed.
    MessagePackDecode(rmp_serde::decode::Error),
    /// A body or frame buffer could not be reserved.
    AllocationFailed { requested: usize },
    /// The UDS decoder is permanently poisoned after a framing/body violation.
    DecoderPoisoned,
    /// A UDS body cannot be represented by its four-byte prefix.
    LengthPrefixOverflow { body_len: usize },
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FrameLimitExceeded {
                frame_limit,
                announced_len: Some(announced_len),
            } => write!(
                formatter,
                "frame length {announced_len} exceeds configured limit {frame_limit}"
            ),
            Self::FrameLimitExceeded {
                frame_limit,
                announced_len: None,
            } => write!(formatter, "frame exceeds configured limit {frame_limit}"),
            Self::EmptyFrame => formatter.write_str("empty UDS frame is invalid"),
            Self::InvalidUtf8(error) => write!(formatter, "frame is not UTF-8: {error}"),
            Self::Json(error) => write!(formatter, "invalid wire JSON: {error}"),
            Self::MessagePackEncode(error) => {
                write!(formatter, "invalid wire MessagePack encode: {error}")
            }
            Self::MessagePackDecode(error) => {
                write!(formatter, "invalid wire MessagePack: {error}")
            }
            Self::AllocationFailed { requested } => {
                write!(formatter, "could not allocate {requested} bytes for frame")
            }
            Self::DecoderPoisoned => formatter.write_str("UDS decoder is poisoned"),
            Self::LengthPrefixOverflow { body_len } => {
                write!(
                    formatter,
                    "frame length {body_len} does not fit a UDS prefix"
                )
            }
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidUtf8(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::MessagePackEncode(error) => Some(error),
            Self::MessagePackDecode(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for CodecError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

struct LimitedWriter {
    bytes: Vec<u8>,
    frame_limit: usize,
    exceeded: bool,
    allocation_failed: Option<usize>,
}

impl LimitedWriter {
    fn new(frame_limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            frame_limit,
            exceeded: false,
            allocation_failed: None,
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(std::io::Error::other("wire frame limit exceeded"));
        };
        if next_len > self.frame_limit {
            self.exceeded = true;
            return Err(std::io::Error::other("wire frame limit exceeded"));
        }
        if next_len > self.bytes.capacity() {
            // Compute a geometric target, then request only that exact target.
            // `try_reserve` may apply its own amortized growth and exceed the
            // frame ceiling; `try_reserve_exact` keeps the codec's cap in
            // control while avoiding per-write O(n²) growth.
            let ceiling = self.frame_limit.max(next_len);
            let target = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(next_len)
                .min(ceiling);
            let additional = target - self.bytes.len();
            if self.bytes.try_reserve_exact(additional).is_err() {
                self.allocation_failed = Some(next_len);
                return Err(std::io::Error::other("wire frame allocation failed"));
            }
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn encode_json(frame: &WireFrame, frame_limit: usize) -> Result<Vec<u8>, CodecError> {
    encode_json_value(frame, frame_limit)
}

pub(crate) fn encode_json_value<T: Serialize + ?Sized>(
    value: &T,
    frame_limit: usize,
) -> Result<Vec<u8>, CodecError> {
    let mut writer = LimitedWriter::new(frame_limit);
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        if writer.exceeded {
            return Err(CodecError::FrameLimitExceeded {
                frame_limit,
                announced_len: None,
            });
        }
        if let Some(requested) = writer.allocation_failed {
            return Err(CodecError::AllocationFailed { requested });
        }
        return Err(CodecError::Json(error));
    }
    Ok(writer.bytes)
}

pub(crate) fn decode_json(bytes: &[u8], frame_limit: usize) -> Result<WireFrame, CodecError> {
    if bytes.len() > frame_limit {
        return Err(CodecError::FrameLimitExceeded {
            frame_limit,
            announced_len: Some(bytes.len()),
        });
    }
    if bytes.is_empty() {
        return Err(CodecError::EmptyFrame);
    }
    Ok(serde_json::from_slice(bytes)?)
}

/// Encodes one post-handshake frame as MessagePack through the same bounded
/// writer used by JSON, so the full oversized body is never accumulated.
pub fn encode_msgpack(frame: &WireFrame, frame_limit: usize) -> Result<Vec<u8>, CodecError> {
    encode_msgpack_value(frame, frame_limit)
}

pub(crate) fn encode_msgpack_value<T: Serialize + ?Sized>(
    value: &T,
    frame_limit: usize,
) -> Result<Vec<u8>, CodecError> {
    let mut writer = LimitedWriter::new(frame_limit);
    let mut serializer = rmp_serde::Serializer::new(&mut writer).with_struct_map();
    if let Err(error) = value.serialize(&mut serializer) {
        if writer.exceeded {
            return Err(CodecError::FrameLimitExceeded {
                frame_limit,
                announced_len: None,
            });
        }
        if let Some(requested) = writer.allocation_failed {
            return Err(CodecError::AllocationFailed { requested });
        }
        return Err(CodecError::MessagePackEncode(error));
    }
    Ok(writer.bytes)
}

/// Decodes one complete post-handshake MessagePack frame after checking its
/// byte length and rejecting zero-length frames identically to JSON.
pub fn decode_msgpack(bytes: &[u8], frame_limit: usize) -> Result<WireFrame, CodecError> {
    if bytes.len() > frame_limit {
        return Err(CodecError::FrameLimitExceeded {
            frame_limit,
            announced_len: Some(bytes.len()),
        });
    }
    if bytes.is_empty() {
        return Err(CodecError::EmptyFrame);
    }
    rmp_serde::from_slice(bytes).map_err(CodecError::MessagePackDecode)
}
