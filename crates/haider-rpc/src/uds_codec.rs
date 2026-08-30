//! UDS framing: four-byte big-endian length plus JSON or MessagePack bytes.
//!
//! The decoder is a streaming state machine and never collects an unbounded
//! staging buffer. It validates the announced length before reserving any body
//! storage. An oversized, empty, or invalid body poisons the decoder; callers
//! must discard it with the connection.

use std::io::Write;
use std::sync::Arc;

use crate::codec::{
    decode_json, decode_msgpack, encode_json, encode_json_value, encode_msgpack,
    encode_msgpack_value,
};
use crate::frame::BorrowedEventFrame;
use crate::{AttachmentId, CodecError, RequestId, WIRE_PROTOCOL_VERSION, WireEncoding, WireFrame};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::SessionId;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

const PREFIX_LEN: usize = 4;

/// One encoded UDS frame kept as its length prefix plus body so the socket
/// writer can issue a vectored write without allocating a second copy.
pub struct ZeroizingEncodedFrame {
    prefix: [u8; PREFIX_LEN],
    body: Zeroizing<Vec<u8>>,
}

/// One `artifact.put` request encoded as small owned JSON/MessagePack
/// bookends around the caller's stable base64 snapshot. The large string is
/// never cloned into a serializer buffer, and every encoded segment is
/// zeroized after the socket write.
pub struct ZeroizingEncodedArtifactPutFrame {
    prefix: [u8; PREFIX_LEN],
    head: Zeroizing<Vec<u8>>,
    data_base64: Arc<Zeroizing<String>>,
    tail: Zeroizing<Vec<u8>>,
}

impl ZeroizingEncodedArtifactPutFrame {
    pub fn framed_len(&self) -> usize {
        PREFIX_LEN
            .saturating_add(self.head.len())
            .saturating_add(self.data_base64.len())
            .saturating_add(self.tail.len())
    }

    pub fn prefix(&self) -> &[u8; PREFIX_LEN] {
        &self.prefix
    }

    pub fn head(&self) -> &[u8] {
        self.head.as_slice()
    }

    pub fn data_base64(&self) -> &[u8] {
        self.data_base64.as_bytes()
    }

    pub fn tail(&self) -> &[u8] {
        self.tail.as_slice()
    }
}

#[derive(Serialize)]
struct BorrowedArtifactPutBody<'a> {
    method: &'static str,
    data_base64: &'a str,
}

#[derive(Serialize)]
struct BorrowedArtifactPutRequest<'a> {
    #[serde(rename = "v")]
    version: u32,
    kind: &'static str,
    request_id: &'a RequestId,
    body: BorrowedArtifactPutBody<'a>,
}

struct CountingWriter {
    len: usize,
    frame_limit: usize,
    exceeded: bool,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(next) = self.len.checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(std::io::Error::other("wire frame length overflow"));
        };
        if next > self.frame_limit {
            self.exceeded = true;
            return Err(std::io::Error::other("wire frame limit exceeded"));
        }
        self.len = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SegmentPhase {
    Head,
    Data,
    Tail,
}

struct SegmentingWriter {
    head: Vec<u8>,
    tail: Vec<u8>,
    data_start: usize,
    data_len: usize,
    data_written: usize,
    overhead_limit: usize,
    phase: SegmentPhase,
    allocation_failed: Option<usize>,
    invalid_data_write: bool,
}

impl SegmentingWriter {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let Some(overhead) = self
            .head
            .len()
            .checked_add(self.tail.len())
            .and_then(|len| len.checked_add(bytes.len()))
        else {
            self.allocation_failed = Some(usize::MAX);
            return Err(std::io::Error::other("wire frame allocation overflow"));
        };
        if overhead > self.overhead_limit {
            self.invalid_data_write = true;
            return Err(std::io::Error::other(
                "serializer copied artifact data instead of borrowing it",
            ));
        }
        let destination = match self.phase {
            SegmentPhase::Head => &mut self.head,
            SegmentPhase::Data | SegmentPhase::Tail => &mut self.tail,
        };
        if destination.try_reserve_exact(bytes.len()).is_err() {
            self.allocation_failed = Some(overhead);
            return Err(std::io::Error::other("wire frame allocation failed"));
        }
        destination.extend_from_slice(bytes);
        Ok(())
    }
}

impl Write for SegmentingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let address = bytes.as_ptr() as usize;
        let data_end = self.data_start.saturating_add(self.data_len);
        let bytes_end = address.saturating_add(bytes.len());
        if address >= self.data_start && bytes_end <= data_end {
            let offset = address - self.data_start;
            if self.phase == SegmentPhase::Tail || offset != self.data_written {
                self.invalid_data_write = true;
                return Err(std::io::Error::other(
                    "serializer emitted artifact data out of order",
                ));
            }
            self.phase = SegmentPhase::Data;
            self.data_written = self.data_written.saturating_add(bytes.len());
        } else {
            if self.phase == SegmentPhase::Data {
                self.phase = SegmentPhase::Tail;
            }
            self.append(bytes)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_artifact_put<W: Write>(
    value: &BorrowedArtifactPutRequest<'_>,
    writer: &mut W,
    encoding: WireEncoding,
) -> Result<(), CodecError> {
    match encoding {
        WireEncoding::Json => serde_json::to_writer(writer, value).map_err(CodecError::Json),
        WireEncoding::MessagePack => {
            let mut serializer = rmp_serde::Serializer::new(writer).with_struct_map();
            value
                .serialize(&mut serializer)
                .map_err(CodecError::MessagePackEncode)
        }
    }
}

/// Counts and then segment-serializes one `artifact.put` request. The first
/// pass performs no allocation and establishes the exact announced body
/// length before the four-byte prefix is constructed.
pub fn encode_artifact_put_request_parts_with(
    request_id: &RequestId,
    data_base64: Arc<Zeroizing<String>>,
    frame_limit: usize,
    encoding: WireEncoding,
) -> Result<ZeroizingEncodedArtifactPutFrame, CodecError> {
    let value = BorrowedArtifactPutRequest {
        version: WIRE_PROTOCOL_VERSION,
        kind: "request",
        request_id,
        body: BorrowedArtifactPutBody {
            method: "artifact.put",
            data_base64: data_base64.as_str(),
        },
    };
    let mut counter = CountingWriter {
        len: 0,
        frame_limit,
        exceeded: false,
    };
    if let Err(error) = serialize_artifact_put(&value, &mut counter, encoding) {
        if counter.exceeded {
            return Err(CodecError::FrameLimitExceeded {
                frame_limit,
                announced_len: None,
            });
        }
        return Err(error);
    }
    let body_len = u32::try_from(counter.len).map_err(|_| CodecError::LengthPrefixOverflow {
        body_len: counter.len,
    })?;
    let overhead_limit =
        counter
            .len
            .checked_sub(data_base64.len())
            .ok_or(CodecError::LengthPrefixOverflow {
                body_len: counter.len,
            })?;
    let mut writer = SegmentingWriter {
        head: Vec::new(),
        tail: Vec::new(),
        data_start: data_base64.as_ptr() as usize,
        data_len: data_base64.len(),
        data_written: 0,
        overhead_limit,
        phase: SegmentPhase::Head,
        allocation_failed: None,
        invalid_data_write: false,
    };
    if let Err(error) = serialize_artifact_put(&value, &mut writer, encoding) {
        if let Some(requested) = writer.allocation_failed {
            return Err(CodecError::AllocationFailed { requested });
        }
        return Err(error);
    }
    if writer.invalid_data_write
        || writer.data_written != data_base64.len()
        || writer.head.len().saturating_add(writer.tail.len()) != overhead_limit
    {
        return Err(CodecError::AllocationFailed {
            requested: counter.len,
        });
    }
    Ok(ZeroizingEncodedArtifactPutFrame {
        prefix: body_len.to_be_bytes(),
        head: Zeroizing::new(writer.head),
        data_base64,
        tail: Zeroizing::new(writer.tail),
    })
}

impl ZeroizingEncodedFrame {
    pub fn framed_len(&self) -> usize {
        PREFIX_LEN + self.body.len()
    }

    pub fn prefix(&self) -> &[u8; PREFIX_LEN] {
        &self.prefix
    }

    pub fn body(&self) -> &[u8] {
        self.body.as_slice()
    }
}

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
    let parts = encode_zeroizing_parts_with(frame, frame_limit, encoding)?;
    let mut body = parts.body;
    let result = (|| {
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
        framed.extend_from_slice(&parts.prefix);
        framed.extend_from_slice(&body);
        Ok(Zeroizing::new(framed))
    })();
    body.zeroize();
    result
}

/// Encodes a frame once while retaining prefix and body as separate buffers.
pub fn encode_zeroizing_parts_with(
    frame: &WireFrame,
    frame_limit: usize,
    encoding: WireEncoding,
) -> Result<ZeroizingEncodedFrame, CodecError> {
    encode_zeroizing_parts_value(frame, frame_limit, encoding)
}

/// Borrowed EVENT encode: exactly the same serializer shape as
/// [`WireFrame::Event`] without cloning its envelope into a logical frame.
pub fn encode_event_zeroizing_parts_with(
    attachment_id: &AttachmentId,
    session_id: &SessionId,
    envelope: &RawEnvelope,
    frame_limit: usize,
    encoding: WireEncoding,
) -> Result<ZeroizingEncodedFrame, CodecError> {
    encode_zeroizing_parts_value(
        &BorrowedEventFrame {
            attachment_id,
            session_id,
            envelope,
        },
        frame_limit,
        encoding,
    )
}

fn encode_zeroizing_parts_value<T: Serialize + ?Sized>(
    value: &T,
    frame_limit: usize,
    encoding: WireEncoding,
) -> Result<ZeroizingEncodedFrame, CodecError> {
    let body = encode_body_value(value, frame_limit, encoding)?;
    let body_len = u32::try_from(body.len()).map_err(|_| CodecError::LengthPrefixOverflow {
        body_len: body.len(),
    })?;
    Ok(ZeroizingEncodedFrame {
        prefix: body_len.to_be_bytes(),
        body: Zeroizing::new(body),
    })
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

fn encode_body_value<T: Serialize + ?Sized>(
    value: &T,
    frame_limit: usize,
    encoding: WireEncoding,
) -> Result<Vec<u8>, CodecError> {
    match encoding {
        WireEncoding::Json => encode_json_value(value, frame_limit),
        WireEncoding::MessagePack => encode_msgpack_value(value, frame_limit),
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
    /// Valid `artifact.put` requests decoded in-place by the daemon-only
    /// decoder mode.
    pub artifact_puts: Vec<InPlaceArtifactPut>,
    /// The terminal error, if this chunk poisoned the decoder.
    pub error: Option<CodecError>,
}

/// One boundary-aware decoder step. A completed frame stops consumption
/// immediately, leaving any suffix for the caller to feed after negotiation.
#[derive(Debug)]
pub struct DecodeStep {
    /// The single frame completed by this step, if any.
    pub frame: Option<WireFrame>,
    /// One daemon-bound `artifact.put` whose base64 bytes were decoded by
    /// reusing the completed decoder allocation.
    pub artifact_put: Option<InPlaceArtifactPut>,
    /// Exact bytes consumed from the supplied chunk.
    pub consumed: usize,
    /// The terminal error, if this step poisoned the decoder.
    pub error: Option<CodecError>,
}

impl DecodeBatch {
    fn complete(frames: Vec<WireFrame>) -> Self {
        Self {
            frames,
            artifact_puts: Vec::new(),
            error: None,
        }
    }

    fn failed(frames: Vec<WireFrame>, error: CodecError) -> Self {
        Self {
            frames,
            artifact_puts: Vec::new(),
            error: Some(error),
        }
    }
}

/// A daemon-bound `artifact.put` parsed without materializing its base64
/// field as a second owned `String`.
pub struct InPlaceArtifactPut {
    pub request_id: RequestId,
    pub bytes: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for InPlaceArtifactPut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InPlaceArtifactPut")
            .field("request_id", &self.request_id)
            .field(
                "bytes",
                &format_args!("[REDACTED; {} bytes]", self.bytes.len()),
            )
            .finish()
    }
}

#[derive(Deserialize)]
struct BorrowedDecodedArtifactPutBody<'a> {
    method: &'a str,
    data_base64: &'a str,
}

#[derive(Deserialize)]
struct BorrowedDecodedArtifactPutRequest<'a> {
    #[serde(rename = "v")]
    version: u32,
    kind: &'a str,
    request_id: &'a str,
    #[serde(borrow)]
    body: BorrowedDecodedArtifactPutBody<'a>,
}

fn decode_artifact_put_in_place(
    body: &mut Vec<u8>,
    encoding: WireEncoding,
) -> Option<InPlaceArtifactPut> {
    let (request_id, data_start, data_len, decoded_len) = {
        let request = match encoding {
            WireEncoding::Json => {
                serde_json::from_slice::<BorrowedDecodedArtifactPutRequest<'_>>(body).ok()?
            }
            WireEncoding::MessagePack => {
                rmp_serde::from_slice::<BorrowedDecodedArtifactPutRequest<'_>>(body).ok()?
            }
        };
        if request.version != WIRE_PROTOCOL_VERSION
            || request.kind != "request"
            || request.body.method != "artifact.put"
        {
            return None;
        }
        let body_start = body.as_ptr() as usize;
        let data_start = (request.body.data_base64.as_ptr() as usize).checked_sub(body_start)?;
        let data_len = request.body.data_base64.len();
        let data_end = data_start.checked_add(data_len)?;
        if data_end > body.len() {
            return None;
        }
        let decoded_len = validated_standard_base64_len(request.body.data_base64.as_bytes())?;
        (
            request.request_id.to_owned(),
            data_start,
            data_len,
            decoded_len,
        )
    };

    let mut input = data_start;
    let input_end = data_start.checked_add(data_len)?;
    let mut output = 0;
    while input < input_end {
        let first = standard_base64_value(body[input])?;
        let second = standard_base64_value(body[input + 1])?;
        let third_padding = body[input + 2] == b'=';
        let fourth_padding = body[input + 3] == b'=';
        let third = if third_padding {
            0
        } else {
            standard_base64_value(body[input + 2])?
        };
        let fourth = if fourth_padding {
            0
        } else {
            standard_base64_value(body[input + 3])?
        };
        body[output] = (first << 2) | (second >> 4);
        output += 1;
        if !third_padding {
            body[output] = (second << 4) | (third >> 2);
            output += 1;
        }
        if !fourth_padding {
            body[output] = (third << 6) | fourth;
            output += 1;
        }
        input += 4;
    }
    if output != decoded_len {
        return None;
    }
    body[decoded_len..].zeroize();
    body.truncate(decoded_len);
    Some(InPlaceArtifactPut {
        request_id: RequestId::new(request_id),
        bytes: Zeroizing::new(std::mem::take(body)),
    })
}

fn validated_standard_base64_len(encoded: &[u8]) -> Option<usize> {
    if !encoded.len().is_multiple_of(4) {
        return None;
    }
    let padding = encoded
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    if padding > 2 {
        return None;
    }
    let content_len = encoded.len().checked_sub(padding)?;
    if encoded[..content_len]
        .iter()
        .any(|byte| standard_base64_value(*byte).is_none())
        || encoded[..content_len].contains(&b'=')
    {
        return None;
    }
    if padding == 2
        && encoded
            .get(content_len.wrapping_sub(1))
            .and_then(|byte| standard_base64_value(*byte))
            .is_none_or(|value| value & 0x0f != 0)
    {
        return None;
    }
    if padding == 1
        && encoded
            .get(content_len.wrapping_sub(1))
            .and_then(|byte| standard_base64_value(*byte))
            .is_none_or(|value| value & 0x03 != 0)
    {
        return None;
    }
    encoded
        .len()
        .checked_div(4)?
        .checked_mul(3)?
        .checked_sub(padding)
}

fn standard_base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
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
    decode_artifact_put_in_place: bool,
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
            decode_artifact_put_in_place: false,
        }
    }

    /// Creates a decoder that additionally zeroizes inbound body buffers
    /// after deserialize (the daemon's sensitive same-UID UDS path).
    pub fn new_zeroizing(frame_limit: usize) -> Self {
        let mut decoder = Self::new(frame_limit);
        decoder.zeroize_bodies = true;
        decoder
    }

    /// Creates the daemon's sensitive decoder. Valid JSON `artifact.put`
    /// requests reuse the completed decoder body for base64 output; every
    /// other frame retains the ordinary typed decode path.
    pub fn new_zeroizing_artifact_put(frame_limit: usize) -> Self {
        let mut decoder = Self::new_zeroizing(frame_limit);
        decoder.decode_artifact_put_in_place = true;
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
            if let Some(artifact_put) = step.artifact_put {
                return DecodeBatch {
                    frames,
                    artifact_puts: vec![artifact_put],
                    error: step.error,
                };
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
                artifact_put: None,
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
                                artifact_put: None,
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
                                    artifact_put: None,
                                    consumed,
                                    error: Some(CodecError::DecoderPoisoned),
                                };
                            }
                        };
                        let in_place = self
                            .decode_artifact_put_in_place
                            .then(|| decode_artifact_put_in_place(&mut body, self.encoding))
                            .flatten();
                        if let Some(artifact_put) = in_place {
                            return DecodeStep {
                                frame: None,
                                artifact_put: Some(artifact_put),
                                consumed,
                                error: None,
                            };
                        }
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
                                    artifact_put: None,
                                    consumed,
                                    error: None,
                                };
                            }
                            Err(error) => {
                                self.poison();
                                return DecodeStep {
                                    frame: None,
                                    artifact_put: None,
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
                        artifact_put: None,
                        consumed,
                        error: Some(CodecError::DecoderPoisoned),
                    };
                }
            }
        }
        DecodeStep {
            frame: None,
            artifact_put: None,
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
