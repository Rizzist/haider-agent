//! UDS framing: four-byte big-endian length plus JSON or MessagePack bytes.
//!
//! The decoder is a streaming state machine and never collects an unbounded
//! staging buffer. It validates the announced length before reserving any body
//! storage. An oversized, empty, or invalid body poisons the decoder; callers
//! must discard it with the connection.

use crate::codec::{decode_json, decode_msgpack, encode_json, encode_msgpack};
use crate::{CodecError, WireEncoding, WireFrame};
use zeroize::{Zeroize, Zeroizing};

const PREFIX_LEN: usize = 4;

/// Serializes one UDS frame: a four-byte big-endian prefix holding the exact
/// JSON body length, followed by the body bytes. This legacy entry point is
/// also used for the always-JSON handshake.
///
/// The frame limit is enforced while serializing, so an oversized frame is
/// rejected before a full-frame buffer ever exists.
pub fn encode(frame: &WireFrame, frame_limit: usize) -> Result<Vec<u8>, CodecError> {
    encode_with(frame, frame_limit, WireEncoding::Json)
}

/// Serializes one UDS frame with the selected post-handshake encoding.
pub fn encode_with(
    frame: &WireFrame,
    frame_limit: usize,
    encoding: WireEncoding,
) -> Result<Vec<u8>, CodecError> {
    let body = encode_body(frame, frame_limit, encoding)?;
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

/// Sensitive encode path (R7): identical bytes to [`encode`], but the
/// intermediate body buffer is zeroized here and the returned framed
/// buffer zeroizes itself on drop — a writer that drops it after the socket
/// write leaves no plaintext copy of a staged secret in freed memory.
///
/// Residual, stated honestly: serialization may reallocate while growing its
/// buffer; bytes left in a superseded allocation are not reachable for
/// scrubbing (the same residual every `Vec`-backed serializer has).
pub fn encode_zeroizing(
    frame: &WireFrame,
    frame_limit: usize,
) -> Result<Zeroizing<Vec<u8>>, CodecError> {
    encode_zeroizing_with(frame, frame_limit, WireEncoding::Json)
}

/// Sensitive encode path using the selected post-handshake encoding.
pub fn encode_zeroizing_with(
    frame: &WireFrame,
    frame_limit: usize,
    encoding: WireEncoding,
) -> Result<Zeroizing<Vec<u8>>, CodecError> {
    let mut body = encode_body(frame, frame_limit, encoding)?;
    let result = (|| {
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
        Ok(Zeroizing::new(framed))
    })();
    body.zeroize();
    result
}

fn encode_body(
    frame: &WireFrame,
    frame_limit: usize,
    encoding: WireEncoding,
) -> Result<Vec<u8>, CodecError> {
    match encoding {
        WireEncoding::Json => encode_json(frame, frame_limit),
        WireEncoding::MessagePack => encode_msgpack(frame, frame_limit),
    }
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

/// One boundary-aware decoder step. A completed frame stops consumption
/// immediately, leaving any suffix for the caller to feed after negotiation.
#[derive(Debug)]
pub struct DecodeStep {
    /// The single frame completed by this step, if any.
    pub frame: Option<WireFrame>,
    /// Exact bytes consumed from the supplied chunk.
    pub consumed: usize,
    /// The terminal error, if this step poisoned the decoder.
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
    /// Sensitive inbound path (R7): zeroize every completed body buffer
    /// after deserialize, and any partial body on drop, so a staged secret's
    /// wire bytes do not linger in freed decoder memory.
    zeroize_bodies: bool,
    encoding: WireEncoding,
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
            zeroize_bodies: false,
            encoding: WireEncoding::Json,
        }
    }

    /// Creates a decoder that additionally zeroizes inbound body buffers
    /// after deserialize (the daemon's sensitive same-UID UDS path).
    pub fn new_zeroizing(frame_limit: usize) -> Self {
        let mut decoder = Self::new(frame_limit);
        decoder.zeroize_bodies = true;
        decoder
    }

    /// Creates a decoder for the selected post-handshake encoding.
    pub fn new_with_encoding(frame_limit: usize, encoding: WireEncoding) -> Self {
        let mut decoder = Self::new(frame_limit);
        decoder.encoding = encoding;
        decoder
    }

    /// Creates a zeroizing decoder for the selected post-handshake encoding.
    pub fn new_zeroizing_with_encoding(frame_limit: usize, encoding: WireEncoding) -> Self {
        let mut decoder = Self::new_zeroizing(frame_limit);
        decoder.encoding = encoding;
        decoder
    }

    /// Switches the post-handshake decoder encoding.
    ///
    /// Callers must switch only at a frame boundary, immediately after the
    /// JSON Hello (daemon) or Welcome (client) has been fully decoded.
    pub fn set_encoding(&mut self, encoding: WireEncoding) {
        if matches!(&self.state, DecodeState::Prefix { filled: 0, .. }) {
            self.encoding = encoding;
        } else {
            self.poison();
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
        let mut frames = Vec::new();
        while !chunk.is_empty() {
            let step = self.push_one(chunk);
            chunk = &chunk[step.consumed..];
            if let Some(frame) = step.frame {
                frames.push(frame);
            }
            if let Some(error) = step.error {
                return DecodeBatch::failed(frames, error);
            }
            if step.consumed == 0 {
                break;
            }
        }
        if self.is_poisoned() {
            DecodeBatch::failed(frames, CodecError::DecoderPoisoned)
        } else {
            DecodeBatch::complete(frames)
        }
    }

    /// Consume through at most one completed frame. The decoder stops at the
    /// clean prefix boundary after that frame even when `chunk` contains more
    /// bytes, so a handshake caller can switch codecs before offering the
    /// suffix.
    pub fn push_one(&mut self, chunk: &[u8]) -> DecodeStep {
        if self.is_poisoned() {
            return DecodeStep {
                frame: None,
                consumed: 0,
                error: Some(CodecError::DecoderPoisoned),
            };
        }

        let mut consumed = 0;
        while consumed < chunk.len() {
            match &mut self.state {
                DecodeState::Prefix { bytes, filled } => {
                    let remaining = PREFIX_LEN - *filled;
                    let take = remaining.min(chunk.len() - consumed);
                    bytes[*filled..*filled + take]
                        .copy_from_slice(&chunk[consumed..consumed + take]);
                    *filled += take;
                    consumed += take;

                    if *filled == PREFIX_LEN {
                        let announced_len = u32::from_be_bytes(*bytes) as usize;
                        if let Err(error) = self.start_body(announced_len) {
                            return DecodeStep {
                                frame: None,
                                consumed,
                                error: Some(error),
                            };
                        }
                    }
                }
                DecodeState::Body {
                    announced_len,
                    bytes,
                } => {
                    let remaining = *announced_len - bytes.len();
                    let take = remaining.min(chunk.len() - consumed);
                    bytes.extend_from_slice(&chunk[consumed..consumed + take]);
                    consumed += take;

                    if bytes.len() == *announced_len {
                        let mut body = match std::mem::replace(
                            &mut self.state,
                            DecodeState::Prefix {
                                bytes: [0; PREFIX_LEN],
                                filled: 0,
                            },
                        ) {
                            DecodeState::Body { bytes, .. } => bytes,
                            _ => {
                                self.poison();
                                return DecodeStep {
                                    frame: None,
                                    consumed,
                                    error: Some(CodecError::DecoderPoisoned),
                                };
                            }
                        };
                        let decoded = match self.encoding {
                            WireEncoding::Json => match std::str::from_utf8(&body) {
                                Ok(_) => decode_json(&body, self.frame_limit),
                                Err(error) => Err(CodecError::InvalidUtf8(error)),
                            },
                            WireEncoding::MessagePack => decode_msgpack(&body, self.frame_limit),
                        };
                        if self.zeroize_bodies {
                            body.zeroize();
                        }
                        match decoded {
                            Ok(frame) => {
                                return DecodeStep {
                                    frame: Some(frame),
                                    consumed,
                                    error: None,
                                };
                            }
                            Err(error) => {
                                self.poison();
                                return DecodeStep {
                                    frame: None,
                                    consumed,
                                    error: Some(error),
                                };
                            }
                        }
                    }
                }
                DecodeState::Poisoned => {
                    return DecodeStep {
                        frame: None,
                        consumed,
                        error: Some(CodecError::DecoderPoisoned),
                    };
                }
            }
        }
        DecodeStep {
            frame: None,
            consumed,
            error: None,
        }
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

    fn poison(&mut self) {
        if self.zeroize_bodies
            && let DecodeState::Body { bytes, .. } = &mut self.state
        {
            bytes.zeroize();
        }
        self.state = DecodeState::Poisoned;
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // Sensitive path: a decoder discarded mid-frame (connection close)
        // must not leave a partially buffered secret body behind.
        if self.zeroize_bodies
            && let DecodeState::Body { bytes, .. } = &mut self.state
        {
            bytes.zeroize();
        }
    }
}
