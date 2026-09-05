//! Store-private text indirection. Protocol envelopes never contain references:
//! every journal reader hydrates this record before exposing an envelope.
//!
//! The reserved MessagePack byte 0xc1 distinguishes these versioned records
//! from legacy named-envelope MessagePack without reserving any user JSON key.

use super::{Connection, FileCas, RawEnvelope, RawPayload, StoreResult};
use crate::{Cas, store_error};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::ArtifactRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

/// Small events retain their original encoding and avoid filesystem work.
/// At this boundary the 71-byte digest plus path metadata is substantially
/// smaller than the text it replaces; equality intentionally externalizes.
const TEXT_CAS_THRESHOLD: usize = 64 * 1_024;
const RECORD_PREFIX: &[u8] = b"\xc1haider.text-cas\x01";
// The following fixed-width decimal field lets metadata-only SQL readers
// retain their logical envelope-byte budget without loading CAS text.
const LENGTH_HEADER_BYTES: usize = 20;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextObject {
    digest: ArtifactRef,
    byte_len: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextField {
    /// RFC 6901 pointer relative to the payload, including array indices.
    path: String,
    object: TextObject,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEnvelope {
    envelope: RawEnvelope,
    reply: Option<TextObject>,
    strings: Vec<TextField>,
}

fn profile_cas(connection: &Connection) -> StoreResult<Option<FileCas>> {
    let Some(path) = connection.path().filter(|path| !path.is_empty()) else {
        // In-memory stores have no durable filesystem namespace. Keeping their
        // ordinary inline encoding also preserves existing migration fixtures.
        return Ok(None);
    };
    let root = Path::new(path).parent().ok_or_else(|| {
        store_error(
            ErrorCode::Internal,
            "journal database has no profile directory",
            false,
        )
    })?;
    FileCas::open(root).map(Some)
}

fn has_large_string(value: &Value) -> bool {
    match value {
        Value::String(text) => text.len() >= TEXT_CAS_THRESHOLD,
        Value::Array(values) => values.iter().any(has_large_string),
        Value::Object(fields) => fields.values().any(has_large_string),
        _ => false,
    }
}

/// Returns a compact private record only when indirection actually applies.
/// Arena-backed replies stream directly into CAS without flattening them.
pub(super) fn encode(
    connection: &Connection,
    envelope: &RawEnvelope,
) -> StoreResult<Option<Vec<u8>>> {
    let large_reply = envelope
        .payload
        .reply_text()
        .filter(|text| text.len() >= TEXT_CAS_THRESHOLD);
    if large_reply.is_none() && !has_large_string(&envelope.payload) {
        return Ok(None);
    }
    let Some(cas) = profile_cas(connection)? else {
        return Ok(None);
    };
    let reply = if let Some(text) = large_reply {
        let byte_len = u64::try_from(text.len()).map_err(|_| {
            store_error(
                ErrorCode::InvalidArgument,
                "reply length exceeds u64",
                false,
            )
        })?;
        let mut upload = cas.begin_put(byte_len)?;
        text.write_to(&mut upload).map_err(|error| {
            store_error(
                ErrorCode::Internal,
                format!("stream journal reply to CAS: {error}"),
                false,
            )
        })?;
        Some(TextObject {
            digest: upload.finish_computed()?,
            byte_len,
        })
    } else {
        None
    };
    let mut strings = Vec::new();
    // RawPayload dereferences to its reply-free skeleton. Borrowing it avoids
    // cloning large generic strings, even when they share a payload with an
    // arena reply. Re-promote the small skeleton and bind its reply below.
    let skeleton = externalize(&envelope.payload, &cas, "", &mut strings)?;
    let mut payload: RawPayload = skeleton.into();
    if let Some(text) = envelope.payload.reply_text() {
        let text = if reply.is_some() {
            Default::default()
        } else {
            text.clone()
        };
        if !payload.replace_reply_text(text) {
            return Err(store_error(
                ErrorCode::Internal,
                "journal reply path disappeared",
                false,
            ));
        }
    }
    let record = StoredEnvelope {
        envelope: RawEnvelope {
            schema_version: envelope.schema_version,
            event_id: envelope.event_id.clone(),
            seq: envelope.seq,
            session_id: envelope.session_id.clone(),
            branch_id: envelope.branch_id.clone(),
            run_id: envelope.run_id.clone(),
            agent_id: envelope.agent_id.clone(),
            device_id: envelope.device_id.clone(),
            authority_epoch: envelope.authority_epoch,
            worker_generation: envelope.worker_generation,
            causation_id: envelope.causation_id.clone(),
            correlation_id: envelope.correlation_id.clone(),
            committed_at_ms: envelope.committed_at_ms,
            render: envelope.render,
            payload,
        },
        reply,
        strings,
    };
    let logical_len = super::encoded_envelope_len(envelope).map_err(|error| {
        store_error(
            ErrorCode::Internal,
            format!("measure CAS-backed envelope: {error}"),
            false,
        )
    })?;
    let mut encoded = RECORD_PREFIX.to_vec();
    encoded.extend_from_slice(format!("{logical_len:020}").as_bytes());
    rmp_serde::encode::write_named(&mut encoded, &record).map_err(|error| {
        store_error(
            ErrorCode::Internal,
            format!("encode CAS-backed journal envelope: {error}"),
            false,
        )
    })?;
    Ok(Some(encoded))
}

fn externalize(
    value: &Value,
    cas: &FileCas,
    path: &str,
    strings: &mut Vec<TextField>,
) -> StoreResult<Value> {
    Ok(match value {
        Value::String(text) if text.len() >= TEXT_CAS_THRESHOLD => {
            strings.push(TextField {
                path: path.to_owned(),
                object: TextObject {
                    digest: cas.put(text.as_bytes())?,
                    byte_len: text.len() as u64,
                },
            });
            Value::String(String::new())
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| externalize(value, cas, &format!("{path}/{index}"), strings))
                .collect::<StoreResult<_>>()?,
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let component = key.replace('~', "~0").replace('/', "~1");
                    Ok((
                        key.clone(),
                        externalize(value, cas, &format!("{path}/{component}"), strings)?,
                    ))
                })
                .collect::<StoreResult<_>>()?,
        ),
        _ => value.clone(),
    })
}

pub(super) fn decode(connection: &Connection, bytes: &[u8]) -> Result<RawEnvelope, String> {
    let Some(encoded) = bytes.strip_prefix(RECORD_PREFIX) else {
        return rmp_serde::from_slice(bytes)
            .map_err(|error| format!("MessagePack decode failed: {error}"));
    };
    let (length_header, encoded) = encoded
        .split_at_checked(LENGTH_HEADER_BYTES)
        .ok_or_else(|| "CAS-backed envelope length header is truncated".to_owned())?;
    if !length_header.iter().all(u8::is_ascii_digit) {
        return Err("CAS-backed envelope length header is not decimal".to_owned());
    }
    let logical_len: usize = std::str::from_utf8(length_header)
        .map_err(|error| error.to_string())?
        .parse()
        .map_err(|error| format!("CAS-backed envelope length is invalid: {error}"))?;
    let mut stored: StoredEnvelope = rmp_serde::from_slice(encoded)
        .map_err(|error| format!("CAS-backed envelope decode failed: {error}"))?;
    if stored.reply.is_none() && stored.strings.is_empty() {
        return Err("CAS-backed envelope contains no text references".to_owned());
    }
    let cas = profile_cas(connection)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "CAS-backed envelope has no profile namespace".to_owned())?;
    if !stored.strings.is_empty() {
        let mut payload = stored.envelope.payload.to_json_value();
        let mut seen = HashSet::new();
        for field in stored.strings {
            if !seen.insert(field.path.clone()) {
                return Err("CAS-backed envelope repeats a text path".to_owned());
            }
            let slot = payload.pointer_mut(&field.path).ok_or_else(|| {
                format!("CAS-backed envelope text path is absent: {}", field.path)
            })?;
            if slot.as_str() != Some("") {
                return Err("CAS-backed envelope text placeholder is not empty".to_owned());
            }
            *slot = Value::String(read_text(&cas, &field.object)?);
        }
        stored.envelope.payload = payload.into();
    }
    if let Some(reply) = stored.reply {
        if stored
            .envelope
            .payload
            .reply_text()
            .is_none_or(|text| !text.is_empty())
        {
            return Err("CAS-backed envelope reply placeholder is invalid".to_owned());
        }
        if !stored
            .envelope
            .payload
            .replace_reply_text(read_text(&cas, &reply)?.into())
        {
            return Err("CAS-backed envelope reply cannot be hydrated".to_owned());
        }
    }
    let actual_len = super::encoded_envelope_len(&stored.envelope)
        .map_err(|error| format!("measure hydrated CAS-backed envelope: {error}"))?;
    if actual_len != logical_len {
        return Err("CAS-backed envelope logical length differs after hydration".to_owned());
    }
    Ok(stored.envelope)
}

fn read_text(cas: &FileCas, object: &TextObject) -> Result<String, String> {
    if object.byte_len < TEXT_CAS_THRESHOLD as u64 {
        return Err("CAS-backed text length is below the storage threshold".to_owned());
    }
    // Verify before returning any bytes. Reuse the same descriptor after its
    // digest scan, so replacing a path cannot change which object is decoded.
    let mut file = cas
        .open_verified(&object.digest)
        .map_err(|error| error.to_string())?;
    let actual_len = file.metadata().map_err(|error| error.to_string())?.len();
    if actual_len != object.byte_len {
        return Err("CAS-backed text length differs from its stored descriptor".to_owned());
    }
    let byte_len = usize::try_from(actual_len)
        .map_err(|_| "CAS-backed text length exceeds address space".to_owned())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(byte_len)
        .map_err(|error| format!("cannot allocate hydrated CAS text: {error}"))?;
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() != byte_len
        || format!("blake3:{}", blake3::hash(&bytes).to_hex()) != object.digest.as_str()
    {
        return Err("CAS-backed text changed while hydrating".to_owned());
    }
    String::from_utf8(bytes).map_err(|error| format!("CAS-backed text is not UTF-8: {error}"))
}

#[cfg(test)]
#[path = "event_text_cas_tests.rs"]
mod tests;
