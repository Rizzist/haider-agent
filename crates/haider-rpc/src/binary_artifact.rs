//! Additive artifact transport on the existing length-prefixed byte stream.
//! The prefix high bit distinguishes binary bodies after feature negotiation.
//! A connection has one upload at a time; only Finish publishes its digest.
use crate::{CodecError, RequestId};
use haider_protocol::ids::ArtifactRef;
use zeroize::Zeroizing;

pub const FEATURE: &str = "artifact_put_binary_v1";
pub const MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_FRAME_BYTES: usize = CHUNK_BYTES + 138;
pub const PREFIX_FLAG: u32 = 1 << 31;

pub enum Frame {
    Begin {
        request_id: RequestId,
        bytes: u64,
        digest: ArtifactRef,
    },
    Chunk {
        request_id: RequestId,
        offset: u64,
        bytes: Zeroizing<Vec<u8>>,
    },
    Finish {
        request_id: RequestId,
    },
}

fn invalid() -> CodecError {
    CodecError::InvalidBinaryFrame
}

pub fn encode(frame: &Frame, limit: usize) -> Result<Zeroizing<Vec<u8>>, CodecError> {
    let mut body = Zeroizing::new(Vec::new());
    let request_id = match frame {
        Frame::Begin { request_id, .. }
        | Frame::Chunk { request_id, .. }
        | Frame::Finish { request_id } => request_id,
    };
    let id = request_id.as_str().as_bytes();
    if id.is_empty() || id.len() > 128 {
        return Err(invalid());
    }
    body.push(id.len() as u8);
    body.extend_from_slice(id);
    match frame {
        Frame::Begin {
            request_id,
            bytes,
            digest,
        } => {
            let _ = request_id;
            let hash = digest
                .as_str()
                .strip_prefix("blake3:")
                .ok_or_else(invalid)?;
            if id.is_empty()
                || id.len() > 128
                || *bytes > MAX_BYTES
                || hash.len() != 64
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(invalid());
            }
            body.push(1);
            body.extend_from_slice(&bytes.to_be_bytes());
            body.extend_from_slice(hash.as_bytes());
        }
        Frame::Chunk { offset, bytes, .. } => {
            if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
                return Err(invalid());
            }
            body.push(2);
            body.extend_from_slice(&offset.to_be_bytes());
            body.extend_from_slice(bytes);
        }
        Frame::Finish { .. } => body.push(3),
    }
    let cap = limit.min(MAX_FRAME_BYTES);
    if body.len() > cap {
        return Err(CodecError::FrameLimitExceeded {
            frame_limit: cap,
            announced_len: Some(body.len()),
        });
    }
    let mut framed = Zeroizing::new(Vec::with_capacity(body.len() + 4));
    framed.extend_from_slice(&((body.len() as u32) | PREFIX_FLAG).to_be_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

pub(crate) fn decode(body: &[u8]) -> Result<Frame, CodecError> {
    let id_len = *body.first().ok_or_else(invalid)? as usize;
    if id_len == 0 || id_len > 128 || body.len() <= 1 + id_len {
        return Err(invalid());
    }
    let request_id =
        RequestId::new(std::str::from_utf8(&body[1..1 + id_len]).map_err(|_| invalid())?);
    let body = &body[1 + id_len..];
    match body.first() {
        Some(1) if body.len() == 73 => {
            let bytes = u64::from_be_bytes(body[1..9].try_into().map_err(|_| invalid())?);
            let hash = std::str::from_utf8(&body[9..73]).map_err(|_| invalid())?;
            if bytes > MAX_BYTES
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(invalid());
            }
            Ok(Frame::Begin {
                request_id,
                bytes,
                digest: ArtifactRef::new(format!("blake3:{hash}")),
            })
        }
        Some(2) if (10..=CHUNK_BYTES + 9).contains(&body.len()) => Ok(Frame::Chunk {
            request_id,
            offset: u64::from_be_bytes(body[1..9].try_into().map_err(|_| invalid())?),
            bytes: Zeroizing::new(body[9..].to_vec()),
        }),
        Some(3) if body.len() == 1 => Ok(Frame::Finish { request_id }),
        _ => Err(invalid()),
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Begin {
                request_id, bytes, ..
            } => f
                .debug_struct("BinaryBegin")
                .field("request_id", request_id)
                .field("bytes", bytes)
                .finish_non_exhaustive(),
            Self::Chunk {
                request_id,
                offset,
                bytes,
            } => f
                .debug_struct("BinaryChunk")
                .field("request_id", request_id)
                .field("offset", offset)
                .field("bytes", &bytes.len())
                .finish_non_exhaustive(),
            Self::Finish { request_id } => f.debug_tuple("BinaryFinish").field(request_id).finish(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{WireEncoding, uds_codec::Decoder};

    #[test]
    fn binary_artifact_roundtrips_at_every_split_in_both_encodings() {
        let frames = [
            Frame::Begin {
                request_id: RequestId::new("begin"),
                bytes: 264 * 1024 * 1024,
                digest: ArtifactRef::new(format!("blake3:{}", "a".repeat(64))),
            },
            Frame::Chunk {
                request_id: RequestId::new("chunk"),
                offset: 123,
                bytes: Zeroizing::new(vec![0, 255, b'{', b'\n']),
            },
            Frame::Finish {
                request_id: RequestId::new("finish"),
            },
        ];
        for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
            for frame in &frames {
                let bytes = encode(frame, MAX_FRAME_BYTES).expect("encode");
                for split in 0..=bytes.len() {
                    let mut decoder =
                        Decoder::new_zeroizing_with_encoding(MAX_FRAME_BYTES, encoding);
                    decoder.set_binary_artifacts(true);
                    let mut first = decoder.push(&bytes[..split]);
                    let second = decoder.push(&bytes[split..]);
                    assert!(first.error.is_none() && second.error.is_none());
                    first.binary_artifacts.extend(second.binary_artifacts);
                    assert_eq!(first.binary_artifacts.len(), 1);
                    assert_eq!(
                        *encode(&first.binary_artifacts[0], MAX_FRAME_BYTES).expect("reencode"),
                        *bytes
                    );
                }
            }
        }
    }

    #[test]
    fn binary_artifact_rejects_unnegotiated_oversized_and_invalid_bodies() {
        let bytes = encode(
            &Frame::Finish {
                request_id: RequestId::new("f"),
            },
            MAX_FRAME_BYTES,
        )
        .expect("encode");
        let mut decoder = Decoder::new(MAX_FRAME_BYTES);
        assert!(decoder.push(&bytes[..4]).error.is_some());
        assert!(decoder.is_poisoned());
        let mut decoder = Decoder::new(usize::MAX);
        decoder.set_binary_artifacts(true);
        let prefix = (PREFIX_FLAG | (MAX_FRAME_BYTES as u32 + 1)).to_be_bytes();
        assert!(matches!(
            decoder.push(&prefix).error,
            Some(CodecError::FrameLimitExceeded { .. })
        ));
        let mut invalid = bytes.to_vec();
        *invalid.last_mut().expect("body") = 255;
        let mut decoder = Decoder::new(MAX_FRAME_BYTES);
        decoder.set_binary_artifacts(true);
        assert!(decoder.push(&invalid).error.is_some());
        assert!(decoder.is_poisoned());
    }

    #[test]
    fn binary_artifact_partial_frame_does_not_deliver_and_debug_redacts() {
        let frame = Frame::Chunk {
            request_id: RequestId::new("chunk"),
            offset: 0,
            bytes: Zeroizing::new(b"secret-content".to_vec()),
        };
        let bytes = encode(&frame, MAX_FRAME_BYTES).expect("encode");
        let mut decoder = Decoder::new_zeroizing(MAX_FRAME_BYTES);
        decoder.set_binary_artifacts(true);
        assert!(
            decoder
                .push(&bytes[..bytes.len() - 1])
                .binary_artifacts
                .is_empty()
        );
        assert!(!format!("{frame:?}").contains("secret-content"));
    }
}
