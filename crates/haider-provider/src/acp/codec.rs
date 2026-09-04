//! Newline-delimited JSON framing for the ACP stdio transport.
//!
//! The transport rule is verbatim: "Messages are delimited by newlines
//! (`\n`), and MUST NOT contain embedded newlines." This is NOT
//! `Content-Length` framing, so the decoder is a line splitter with a bound.
//!
//! Two tolerances are deliberate and both come from the wire-facts document:
//! a blank line is skipped, and a line that is not valid JSON is reported as a
//! RECOVERABLE error carrying no frame content — Antigravity 1.1.1 prints its
//! OAuth URL on stderr, but earlier builds printed it on stdout, so a
//! non-JSON stdout line must never take the connection down and must never
//! reach a log.

use crate::acp::wire::InboundFrame;

/// Maximum bytes accepted in one newline-delimited ACP frame, in either
/// direction.
///
/// Derivation. The largest object Haider can put on this wire is one
/// `session/prompt` carrying the composer's per-turn attachment budget, and
/// ACP carries image content as base64 INSIDE the single JSON line:
///
/// - `haider_protocol::tool::TOOL_RESULT_IMAGE_MAX_BYTES_PER_TURN` is the
///   encoded-image ceiling admitted to one provider request: 16 MiB.
/// - base64 expands 4/3, and its alphabet contains no JSON-escapable byte, so
///   the string cannot expand further: 16 MiB * 4 / 3 = 21.33 MiB.
/// - the composer's own text ceiling is `haider_rpc::SURFACE_INPUT_MAX_BYTES`
///   = 64 KiB, and the JSON-RPC envelope plus per-block keys are a few hundred
///   bytes each.
/// - 21.33 MiB + 0.06 MiB + envelope, rounded up to the next power of two, is
///   32 MiB, leaving 32 - 21.33 = 10.67 MiB of headroom for text, envelope and
///   further content blocks.
///
/// The bound is a TRANSPORT ceiling negotiated once per connection, not a
/// per-turn budget, so it must already admit the attachment shapes later
/// slices will send. Peak decoder memory is one frame plus the current read
/// chunk, so this constant is also the decoder's worst-case residency.
pub const ACP_MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// A recoverable framing failure. No variant carries frame content: a
/// malformed line may be an OAuth URL that must never be logged, journalled,
/// or put in an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// A line exceeded [`ACP_MAX_FRAME_BYTES`]. The decoder resynchronizes at
    /// the next newline and never retains the over-long bytes.
    LineTooLong { limit: usize },
    /// The line was not a JSON object Haider can decode as a JSON-RPC frame.
    MalformedJson,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LineTooLong { limit } => write!(
                formatter,
                "the agent sent a line longer than the {limit}-byte ACP frame limit"
            ),
            Self::MalformedJson => {
                formatter.write_str("the agent sent a line that is not a valid ACP message")
            }
        }
    }
}

/// Incremental newline-delimited JSON decoder.
///
/// Handles one message split across many reads and several messages coalesced
/// into one read. Feeding is separate from draining so a caller can push one
/// OS read and then drain every complete frame it produced.
#[derive(Debug)]
pub struct LineFramer {
    buffer: Vec<u8>,
    limit: usize,
    /// Set after an over-long line: bytes are discarded until the next
    /// newline so the decoder resynchronizes without ever buffering past the
    /// limit.
    resynchronizing: bool,
}

impl Default for LineFramer {
    fn default() -> Self {
        Self::new()
    }
}

impl LineFramer {
    pub fn new() -> Self {
        Self::with_limit(ACP_MAX_FRAME_BYTES)
    }

    pub fn with_limit(limit: usize) -> Self {
        Self {
            buffer: Vec::new(),
            limit,
            resynchronizing: false,
        }
    }

    /// Appends one transport read. The buffer never exceeds the limit plus the
    /// size of the chunk that crossed it, because [`Self::next_frame`] drops
    /// the buffer as soon as it observes the overrun.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// Yields the next complete frame, or `None` when the buffered bytes do
    /// not yet contain one.
    pub fn next_frame(&mut self) -> Option<Result<InboundFrame, FrameError>> {
        loop {
            if self.resynchronizing {
                match memchr_newline(&self.buffer) {
                    Some(index) => {
                        self.buffer.drain(..=index);
                        self.resynchronizing = false;
                    }
                    None => {
                        self.buffer.clear();
                        return None;
                    }
                }
                continue;
            }

            let Some(index) = memchr_newline(&self.buffer) else {
                if self.buffer.len() > self.limit {
                    self.buffer.clear();
                    self.resynchronizing = true;
                    return Some(Err(FrameError::LineTooLong { limit: self.limit }));
                }
                return None;
            };

            if index > self.limit {
                self.buffer.drain(..=index);
                return Some(Err(FrameError::LineTooLong { limit: self.limit }));
            }

            let line: Vec<u8> = self.buffer.drain(..=index).take(index).collect();
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return Some(
                serde_json::from_slice::<InboundFrame>(&line)
                    .map_err(|_| FrameError::MalformedJson),
            );
        }
    }

    #[cfg(test)]
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}

fn memchr_newline(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|byte| *byte == b'\n')
}

/// Encodes one outbound frame as a single newline-terminated line.
///
/// `serde_json` emits compact JSON in which every in-string newline is escaped
/// as `\n`, so the produced line can only contain the one terminator. The scan
/// is kept anyway: the "MUST NOT contain embedded newlines" rule is the whole
/// framing contract, and a violation would silently desynchronize the agent
/// rather than fail loudly.
pub fn encode_frame<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| FrameError::MalformedJson)?;
    if bytes.len() > ACP_MAX_FRAME_BYTES {
        return Err(FrameError::LineTooLong {
            limit: ACP_MAX_FRAME_BYTES,
        });
    }
    if memchr_newline(&bytes).is_some() {
        return Err(FrameError::MalformedJson);
    }
    bytes.push(b'\n');
    Ok(bytes)
}
