//! Shared JSON codec support.

use std::io::Write;

use crate::WireFrame;

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
    /// A zero-length JSON body is invalid on both transports: an empty WS
    /// text message, or a UDS length prefix announcing zero bytes.
    EmptyFrame,
    /// A complete UDS body was not UTF-8.
    InvalidUtf8(std::str::Utf8Error),
    /// JSON serialization or decoding failed.
    Json(serde_json::Error),
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
        if self.bytes.try_reserve_exact(buffer.len()).is_err() {
            self.allocation_failed = Some(next_len);
            return Err(std::io::Error::other("wire frame allocation failed"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn encode_json(frame: &WireFrame, frame_limit: usize) -> Result<Vec<u8>, CodecError> {
    let mut writer = LimitedWriter::new(frame_limit);
    if let Err(error) = serde_json::to_writer(&mut writer, frame) {
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
