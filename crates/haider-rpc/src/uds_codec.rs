//! UDS framing: four-byte big-endian length plus shared UTF-8 JSON bytes.
//!
//! The decoder is a streaming state machine and never collects an unbounded
//! staging buffer. It validates the announced length before reserving any body
//! storage. An oversized, empty, invalid-UTF-8, or invalid-JSON frame poisons
//! the decoder; callers must discard it with the connection.

use crate::codec::{decode_json, encode_json};
use crate::{CodecError, WireFrame};

const PREFIX_LEN: usize = 4;

/// Serializes one UDS frame: a four-byte big-endian prefix holding the exact
/// JSON body length, followed by the body bytes.
///
/// The frame limit is enforced while serializing, so an oversized frame is
/// rejected before a full-frame buffer ever exists.
pub fn encode(frame: &WireFrame, frame_limit: usize) -> Result<Vec<u8>, CodecError> {
    let body = encode_json(frame, frame_limit)?;
    let body_len = u32::try_from(body.len()).map_err(|_| CodecError::LengthPrefixOverflow {
        body_len: body.len(),
    })?;
    let total_len = PREFIX_LEN
        .checked_add(body.len())
        .ok_or(CodecError::AllocationFailed {
            requested: body.len(),
        })?;
    let mut framed = Vec::new();
    framed
        .try_reserve_exact(total_len)
        .map_err(|_| CodecError::AllocationFailed {
            requested: total_len,
        })?;
    framed.extend_from_slice(&body_len.to_be_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

#[derive(Debug)]
enum DecodeState {
    Prefix {
        bytes: [u8; PREFIX_LEN],
        filled: usize,
    },
    Body {
        announced_len: usize,
        bytes: Vec<u8>,
    },
    Poisoned,
}

/// Result of decoding one arbitrary UDS byte chunk.
///
/// A chunk can finish valid frames and then reveal a framing/body error. Those
/// frames are returned in [`Self::frames`] while the terminal violation is
/// returned in [`Self::error`]. This makes the delivered transcript invariant
/// to OS read boundaries without delaying poisoning: after an error, the
/// decoder is immediately poisoned and all remaining chunk bytes are ignored.
#[derive(Debug)]
pub struct DecodeBatch {
    /// Every valid frame completed before any terminal error in this chunk.
    pub frames: Vec<WireFrame>,
    /// The terminal error, if this chunk poisoned the decoder.
    pub error: Option<CodecError>,
}

impl DecodeBatch {
    fn complete(frames: Vec<WireFrame>) -> Self {
        Self {
            frames,
            error: None,
        }
    }

    fn failed(frames: Vec<WireFrame>, error: CodecError) -> Self {
        Self {
            frames,
            error: Some(error),
        }
    }
}

/// Streaming UDS frame decoder.
#[derive(Debug)]
pub struct Decoder {
    frame_limit: usize,
    state: DecodeState,
}

impl Decoder {
    /// Creates a decoder with a strict JSON-body byte limit.
    pub fn new(frame_limit: usize) -> Self {
        Self {
            frame_limit,
            state: DecodeState::Prefix {
                bytes: [0; PREFIX_LEN],
                filled: 0,
            },
        }
    }

    /// Returns whether a prior protocol violation permanently poisoned this
    /// decoder.
    pub fn is_poisoned(&self) -> bool {
        matches!(self.state, DecodeState::Poisoned)
    }

    /// Accepts an arbitrary byte chunk and returns every completed frame plus
    /// any terminal error encountered after those frames.
    ///
    /// Any framing or body violation permanently poisons the decoder
    /// (poisoned, not recoverable, is the documented choice); the caller
    /// delivers `frames`, observes `error`, then discards the decoder with its
    /// connection.
    pub fn push(&mut self, mut chunk: &[u8]) -> DecodeBatch {
        if self.is_poisoned() {
            return DecodeBatch::failed(Vec::new(), CodecError::DecoderPoisoned);
        }

        let mut frames = Vec::new();
        while !chunk.is_empty() {
            match &mut self.state {
                DecodeState::Prefix { bytes, filled } => {
                    let remaining = PREFIX_LEN - *filled;
                    let take = remaining.min(chunk.len());
                    bytes[*filled..*filled + take].copy_from_slice(&chunk[..take]);
                    *filled += take;
                    chunk = &chunk[take..];

                    if *filled == PREFIX_LEN {
                        let announced_len = u32::from_be_bytes(*bytes) as usize;
                        if let Err(error) = self.start_body(announced_len) {
                            return DecodeBatch::failed(frames, error);
                        }
                    }
                }
                DecodeState::Body {
                    announced_len,
                    bytes,
                } => {
                    let remaining = *announced_len - bytes.len();
                    let take = remaining.min(chunk.len());
                    bytes.extend_from_slice(&chunk[..take]);
                    chunk = &chunk[take..];

                    if bytes.len() == *announced_len {
                        let body = match std::mem::replace(
                            &mut self.state,
                            DecodeState::Prefix {
                                bytes: [0; PREFIX_LEN],
                                filled: 0,
                            },
                        ) {
                            DecodeState::Body { bytes, .. } => bytes,
                            _ => {
                                self.state = DecodeState::Poisoned;
                                return DecodeBatch::failed(frames, CodecError::DecoderPoisoned);
                            }
                        };
                        match std::str::from_utf8(&body) {
                            Ok(_) => match decode_json(&body, self.frame_limit) {
                                Ok(frame) => frames.push(frame),
                                Err(error) => {
                                    self.state = DecodeState::Poisoned;
                                    return DecodeBatch::failed(frames, error);
                                }
                            },
                            Err(error) => {
                                self.state = DecodeState::Poisoned;
                                return DecodeBatch::failed(frames, CodecError::InvalidUtf8(error));
                            }
                        }
                    }
                }
                DecodeState::Poisoned => {
                    return DecodeBatch::failed(frames, CodecError::DecoderPoisoned);
                }
            }
        }
        DecodeBatch::complete(frames)
    }

    fn start_body(&mut self, announced_len: usize) -> Result<(), CodecError> {
        if announced_len == 0 {
            self.state = DecodeState::Poisoned;
            return Err(CodecError::EmptyFrame);
        }
        if announced_len > self.frame_limit {
            self.state = DecodeState::Poisoned;
            return Err(CodecError::FrameLimitExceeded {
                frame_limit: self.frame_limit,
                announced_len: Some(announced_len),
            });
        }

        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(announced_len).is_err() {
            self.state = DecodeState::Poisoned;
            return Err(CodecError::AllocationFailed {
                requested: announced_len,
            });
        }
        self.state = DecodeState::Body {
            announced_len,
            bytes,
        };
        Ok(())
    }
}
