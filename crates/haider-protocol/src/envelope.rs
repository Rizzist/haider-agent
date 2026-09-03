//! The event envelope — one event stream, many encodings (Thesis 1).
//!
//! Every committed fact rides this envelope. State is a deterministic
//! projection of committed envelopes; subscribers resume from `seq`.
//!
//! Forward-compat policy (frozen): readers MUST tolerate unknown payload kinds
//! by falling back to [`RawEnvelope`] (same envelope fields, payload kept as
//! raw JSON). Writers never remove or re-type existing fields — schema changes
//! bump `schema_version` with an upcaster and golden old/new fixtures.

use crate::EventPayload;
use crate::history::NodeKind;
use crate::ids::{AgentId, BranchId, DeviceId, EventId, RunId, SessionId};
use crate::item::{ItemDelta, ItemEvent, TurnItem};
use crate::reply::ReplyText;
use serde::de::DeserializeOwned;
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::io;
use std::ops::{Deref, DerefMut};

/// Current envelope schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Where an event is rendered (§6.1: three surfaces, never conflated).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderTargets {
    pub ui: bool,
    pub durable: bool,
    pub prompt: PromptRender,
}

/// How (if at all) the prompt compiler may include this event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptRender {
    Verbatim,
    Pruned,
    Omit,
}

/// The envelope. `payload` is typed for known kinds; use [`RawEnvelope`] to
/// read streams that may contain newer kinds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope<P> {
    pub schema_version: u32,
    pub event_id: EventId,
    /// Monotonic per-session sequence, allocated at commit (never by workers).
    pub seq: u64,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    pub device_id: DeviceId,
    /// Single-writer invariant: which authority epoch committed this.
    pub authority_epoch: u64,
    /// Fencing: which worker generation produced it (stale generations rejected).
    pub worker_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<EventId>,
    /// Milliseconds since epoch, assigned at commit.
    pub committed_at_ms: u64,
    pub render: RenderTargets,
    pub payload: P,
}

/// Envelope payload that preserves unknown JSON while keeping large streamed
/// reply leaves as arena ranges.
///
/// `Json` is the unchanged forward-compat representation. `Reply` stores the
/// same canonical JSON tree with its one reply leaf replaced by an empty
/// scalar; serialization patches the shared range back into that exact field.
/// This is deliberately below [`EventPayload`]: unknown payload and item kinds
/// still round-trip without an upcaster.
#[derive(Debug, Clone)]
pub enum RawPayload {
    Json(Value),
    Reply(ReplyPayload),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyPayload {
    skeleton: Value,
    text: ReplyText,
    path: ReplyPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyPath {
    ItemText,
    ItemSummary,
    DeltaText,
    NodeText,
}

impl ReplyPath {
    const fn components(self) -> &'static [&'static str] {
        match self {
            Self::ItemText => &["item", "text"],
            Self::ItemSummary => &["item", "summary"],
            Self::DeltaText => &["delta", "text"],
            Self::NodeText => &["kind", "text"],
        }
    }
}

impl RawPayload {
    /// Converts a locally-produced typed payload without copying reply bytes.
    pub fn from_event(mut payload: EventPayload) -> Result<Self, serde_json::Error> {
        let reply = reply_leaf_mut(&mut payload).map(|(text, path)| {
            let text = std::mem::take(text);
            (text, path)
        });
        let skeleton = serde_json::to_value(payload)?;
        Ok(match reply {
            Some((text, path)) => Self::Reply(ReplyPayload {
                skeleton,
                text,
                path,
            }),
            None => Self::Json(skeleton),
        })
    }

    /// Decodes the known payload union. Reply leaves remain shared arena
    /// handles rather than becoming owned `String`s.
    pub fn decode_event(&self) -> Result<EventPayload, serde_json::Error> {
        match self {
            Self::Json(value) => serde_json::from_value(value.clone()),
            Self::Reply(reply) => {
                let mut payload = serde_json::from_value::<EventPayload>(reply.skeleton.clone())?;
                let Some((text, path)) = reply_leaf_mut(&mut payload) else {
                    return serde_json::from_value(Value::Null);
                };
                if path != reply.path {
                    return serde_json::from_value(Value::Null);
                }
                *text = reply.text.clone();
                Ok(payload)
            }
        }
    }

    /// Compatibility decoder for non-reply payload unions.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.to_json_value())
    }

    /// Materializes a conventional JSON value. Avoid this on live reply paths;
    /// it exists for compatibility and unknown-payload APIs.
    #[must_use]
    pub fn to_json_value(&self) -> Value {
        match self {
            Self::Json(value) => value.clone(),
            Self::Reply(reply) => {
                let mut value = reply.skeleton.clone();
                if let Some(slot) = value_at_path_mut(&mut value, reply.path.components()) {
                    *slot = Value::String(reply.text.to_owned_string());
                }
                value
            }
        }
    }

    #[must_use]
    pub fn reply_text(&self) -> Option<&ReplyText> {
        match self {
            Self::Json(_) => None,
            Self::Reply(reply) => Some(&reply.text),
        }
    }

    /// Rebinds the reply leaf to another canonical arena range without
    /// touching the byte-stable payload skeleton.
    pub fn replace_reply_text(&mut self, text: ReplyText) -> bool {
        let Self::Reply(reply) = self else {
            return false;
        };
        reply.text = text;
        true
    }

    /// Returns a small conventional payload with the reply leaf replaced by
    /// `marker`. Framing layers use this to serialize their exact outer
    /// schema once, split around the marker, and stream the real arena range
    /// between those bookends.
    #[must_use]
    pub fn with_reply_placeholder(&self, marker: &str) -> Option<Self> {
        let Self::Reply(reply) = self else {
            return None;
        };
        let mut skeleton = reply.skeleton.clone();
        *value_at_path_mut(&mut skeleton, reply.path.components())? =
            Value::String(marker.to_owned());
        Some(Self::Json(skeleton))
    }

    #[must_use]
    pub fn type_tag(&self) -> Option<&str> {
        self.get("type").and_then(Value::as_str)
    }

    #[must_use]
    pub fn owned_heap_bytes(&self) -> usize {
        json_owned_heap_bytes(self.deref())
            .saturating_add(self.reply_text().map_or(0, ReplyText::len))
    }
}

fn reply_leaf_mut(payload: &mut EventPayload) -> Option<(&mut ReplyText, ReplyPath)> {
    match payload {
        EventPayload::Item(ItemEvent::Started { item, .. })
        | EventPayload::Item(ItemEvent::Completed { item, .. }) => match item {
            TurnItem::AgentMessage { text } | TurnItem::IncompleteAgentMessage { text, .. } => {
                Some((text, ReplyPath::ItemText))
            }
            TurnItem::Reasoning { summary } => Some((summary, ReplyPath::ItemSummary)),
            _ => None,
        },
        EventPayload::Item(ItemEvent::Delta { delta, .. }) => match delta {
            ItemDelta::Text { text } | ItemDelta::Reasoning { text } => {
                Some((text, ReplyPath::DeltaText))
            }
            _ => None,
        },
        EventPayload::NodeCommitted(node) => match &mut node.kind {
            NodeKind::AssistantCommit { text, .. } => Some((text, ReplyPath::NodeText)),
            _ => None,
        },
        _ => None,
    }
}

fn value_at_path_mut<'a>(value: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    let Some((head, tail)) = path.split_first() else {
        return Some(value);
    };
    value_at_path_mut(value.get_mut(*head)?, tail)
}

fn reply_path(value: &Value) -> Option<ReplyPath> {
    match value.get("type").and_then(Value::as_str) {
        Some("item") => match value.get("event").and_then(Value::as_str) {
            Some("delta") => match value
                .get("delta")
                .and_then(|delta| delta.get("delta"))
                .and_then(Value::as_str)
            {
                Some("text" | "reasoning") => Some(ReplyPath::DeltaText),
                _ => None,
            },
            Some("started" | "completed") => match value
                .get("item")
                .and_then(|item| item.get("item"))
                .and_then(Value::as_str)
            {
                Some("agent_message" | "incomplete_agent_message") => Some(ReplyPath::ItemText),
                Some("reasoning") => Some(ReplyPath::ItemSummary),
                _ => None,
            },
            _ => None,
        },
        Some("node_committed")
            if value
                .get("kind")
                .and_then(|kind| kind.get("kind"))
                .and_then(Value::as_str)
                == Some("assistant_commit") =>
        {
            Some(ReplyPath::NodeText)
        }
        _ => None,
    }
}

fn promote_value(mut value: Value) -> RawPayload {
    let Some(path) = reply_path(&value) else {
        return RawPayload::Json(value);
    };
    let Some(slot) = value_at_path_mut(&mut value, path.components()) else {
        return RawPayload::Json(value);
    };
    let text = match std::mem::take(slot) {
        Value::String(text) => ReplyText::from(text),
        other => {
            *slot = other;
            return RawPayload::Json(value);
        }
    };
    *slot = Value::String(String::new());
    RawPayload::Reply(ReplyPayload {
        skeleton: value,
        text,
        path,
    })
}

impl From<Value> for RawPayload {
    fn from(value: Value) -> Self {
        promote_value(value)
    }
}

impl From<RawPayload> for Value {
    fn from(payload: RawPayload) -> Self {
        payload.to_json_value()
    }
}

impl Deref for RawPayload {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Json(value) => value,
            Self::Reply(reply) => &reply.skeleton,
        }
    }
}

impl DerefMut for RawPayload {
    fn deref_mut(&mut self) -> &mut Self::Target {
        if matches!(self, Self::Reply(_)) {
            *self = Self::Json(self.to_json_value());
        }
        match self {
            Self::Json(value) => value,
            Self::Reply(_) => unreachable!("reply payload was materialized"),
        }
    }
}

impl PartialEq for RawPayload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Json(left), Self::Json(right)) => left == right,
            (Self::Reply(left), Self::Reply(right)) => left == right,
            _ => self.to_json_value() == other.to_json_value(),
        }
    }
}

impl std::fmt::Display for RawPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, formatter)
    }
}

impl Serialize for RawPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Json(value) => value.serialize(serializer),
            Self::Reply(reply) => PatchedValue {
                value: &reply.skeleton,
                path: reply.path.components(),
                text: &reply.text,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for RawPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(promote_value(value))
    }
}

struct PatchedValue<'a> {
    value: &'a Value,
    path: &'a [&'a str],
    text: &'a ReplyText,
}

impl Serialize for PatchedValue<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.path.is_empty() {
            return self.text.serialize(serializer);
        }
        match self.value {
            Value::Object(fields) => {
                let mut map = serializer.serialize_map(Some(fields.len()))?;
                for (key, value) in fields {
                    map.serialize_entry(
                        key,
                        &Self {
                            value,
                            path: if key == self.path[0] {
                                &self.path[1..]
                            } else {
                                &["\0"]
                            },
                            text: self.text,
                        },
                    )?;
                }
                map.end()
            }
            Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&Self {
                        value,
                        path: &["\0"],
                        text: self.text,
                    })?;
                }
                sequence.end()
            }
            other => other.serialize(serializer),
        }
    }
}

/// Envelope with unknown payloads kept as JSON and streamed reply leaves kept
/// as shared arena ranges.
pub type RawEnvelope = EventEnvelope<RawPayload>;

/// Writes the canonical compact JSON representation without flattening a
/// segmented reply range. Non-reply payloads use serde_json directly.
pub fn write_envelope_json(writer: &mut impl io::Write, envelope: &RawEnvelope) -> io::Result<()> {
    let Some(text) = envelope.payload.reply_text() else {
        return serde_json::to_writer(writer, envelope).map_err(io::Error::other);
    };
    let (encoded, token) = encoded_reply_template(envelope, TemplateEncoding::Json)?;
    let offset = unique_subslice_offset(&encoded, &token)?;
    writer.write_all(&encoded[..offset])?;
    writer.write_all(b"\"")?;
    write_json_string_contents(writer, text)?;
    writer.write_all(b"\"")?;
    writer.write_all(&encoded[offset + token.len()..])
}

/// Writes one compact payload JSON value without flattening its reply leaf.
pub fn write_payload_json(writer: &mut impl io::Write, payload: &RawPayload) -> io::Result<()> {
    let Some(text) = payload.reply_text() else {
        return serde_json::to_writer(writer, payload).map_err(io::Error::other);
    };
    const MARKER: &str = "__haider_payload_reply_range__";
    let template = payload
        .with_reply_placeholder(MARKER)
        .ok_or_else(|| io::Error::other("reply payload has no placeholder path"))?;
    let encoded = serde_json::to_vec(&template).map_err(io::Error::other)?;
    let token = serde_json::to_vec(MARKER).map_err(io::Error::other)?;
    let offset = unique_subslice_offset(&encoded, &token)?;
    writer.write_all(&encoded[..offset])?;
    text.write_json_string_to(writer)?;
    writer.write_all(&encoded[offset + token.len()..])
}

/// Writes the canonical named-struct MessagePack representation without
/// building a complete envelope buffer for a segmented reply range.
pub fn write_envelope_messagepack(
    writer: &mut impl io::Write,
    envelope: &RawEnvelope,
) -> io::Result<()> {
    let Some(text) = envelope.payload.reply_text() else {
        return rmp_serde::encode::write_named(writer, envelope).map_err(io::Error::other);
    };
    let (encoded, token) = encoded_reply_template(envelope, TemplateEncoding::MessagePack)?;
    let offset = unique_subslice_offset(&encoded, &token)?;
    writer.write_all(&encoded[..offset])?;
    write_messagepack_string_len(writer, text.len())?;
    text.write_to(writer)?;
    writer.write_all(&encoded[offset + token.len()..])
}

#[derive(Clone, Copy)]
enum TemplateEncoding {
    Json,
    MessagePack,
}

fn encoded_reply_template(
    envelope: &RawEnvelope,
    encoding: TemplateEncoding,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let RawPayload::Reply(reply) = &envelope.payload else {
        return Err(io::Error::other(
            "reply template requested for JSON payload",
        ));
    };
    for nonce in 0_u32..1_024 {
        let marker = format!("__haider_reply_arena_{nonce:08x}_sentinel__");
        let mut skeleton = reply.skeleton.clone();
        let slot = value_at_path_mut(&mut skeleton, reply.path.components())
            .ok_or_else(|| io::Error::other("reply payload path disappeared"))?;
        *slot = Value::String(marker.clone());
        let template = envelope.clone().map_payload(|_| RawPayload::Json(skeleton));
        let (encoded, token) = match encoding {
            TemplateEncoding::Json => (
                serde_json::to_vec(&template).map_err(io::Error::other)?,
                serde_json::to_vec(&marker).map_err(io::Error::other)?,
            ),
            TemplateEncoding::MessagePack => (
                rmp_serde::to_vec_named(&template).map_err(io::Error::other)?,
                rmp_serde::to_vec_named(&marker).map_err(io::Error::other)?,
            ),
        };
        if subslice_count(&encoded, &token) == 1 {
            return Ok((encoded, token));
        }
    }
    Err(io::Error::other(
        "could not choose a unique reply serialization sentinel",
    ))
}

fn unique_subslice_offset(haystack: &[u8], needle: &[u8]) -> io::Result<usize> {
    if needle.is_empty() {
        return Err(io::Error::other("empty reply serialization sentinel"));
    }
    let mut matches = haystack
        .windows(needle.len())
        .enumerate()
        .filter_map(|(offset, window)| (window == needle).then_some(offset));
    let offset = matches
        .next()
        .ok_or_else(|| io::Error::other("reply serialization sentinel disappeared"))?;
    if matches.next().is_some() {
        return Err(io::Error::other(
            "reply serialization sentinel is ambiguous",
        ));
    }
    Ok(offset)
}

fn subslice_count(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn write_json_string_contents(writer: &mut impl io::Write, text: &ReplyText) -> io::Result<()> {
    const WINDOW_BYTES: usize = 16 * 1_024;
    let mut result = Ok(());
    text.visit_strs(|segment| {
        let mut start = 0;
        while result.is_ok() && start < segment.len() {
            let mut end = start.saturating_add(WINDOW_BYTES).min(segment.len());
            while end > start && !segment.is_char_boundary(end) {
                end -= 1;
            }
            let encoded = match serde_json::to_vec(&segment[start..end]) {
                Ok(encoded) => encoded,
                Err(error) => {
                    result = Err(io::Error::other(error));
                    return;
                }
            };
            debug_assert!(encoded.len() >= 2 && encoded[0] == b'\"');
            result = writer.write_all(&encoded[1..encoded.len() - 1]);
            start = end;
        }
    });
    result
}

fn write_messagepack_string_len(writer: &mut impl io::Write, len: usize) -> io::Result<()> {
    if len < 32 {
        writer.write_all(&[0xa0 | u8::try_from(len).expect("fixstr length")])
    } else if let Ok(len) = u8::try_from(len) {
        writer.write_all(&[0xd9, len])
    } else if let Ok(len) = u16::try_from(len) {
        writer.write_all(&[0xda])?;
        writer.write_all(&len.to_be_bytes())
    } else {
        let len = u32::try_from(len)
            .map_err(|_| io::Error::other("reply exceeds MessagePack string length"))?;
        writer.write_all(&[0xdb])?;
        writer.write_all(&len.to_be_bytes())
    }
}

/// Conservative over-approximation of one raw envelope's owned memory.
///
/// This is the common accounting unit for replay pages and live catch-up
/// buffers. It is deliberately independent of wire framing: the negotiated
/// frame limit governs encoded bytes, while this charge includes the fixed
/// envelope value, every owned ID string, and the payload's heap storage.
///
/// The exhaustive destructure is deliberate. Adding an envelope field must
/// fail this estimator at compile time until its weight is classified.
#[must_use]
pub fn envelope_weight_bytes(envelope: &RawEnvelope) -> usize {
    let EventEnvelope {
        schema_version: _,
        event_id,
        seq: _,
        session_id,
        branch_id,
        run_id,
        agent_id,
        device_id,
        authority_epoch: _,
        worker_generation: _,
        causation_id,
        correlation_id,
        committed_at_ms: _,
        render: _,
        payload,
    } = envelope;
    std::mem::size_of::<RawEnvelope>()
        .saturating_add(event_id.as_str().len())
        .saturating_add(session_id.as_str().len())
        .saturating_add(
            branch_id
                .as_ref()
                .map_or(0, |branch_id| branch_id.as_str().len()),
        )
        .saturating_add(run_id.as_ref().map_or(0, |run_id| run_id.as_str().len()))
        .saturating_add(
            agent_id
                .as_ref()
                .map_or(0, |agent_id| agent_id.as_str().len()),
        )
        .saturating_add(device_id.as_str().len())
        .saturating_add(
            causation_id
                .as_ref()
                .map_or(0, |causation_id| causation_id.as_str().len()),
        )
        .saturating_add(
            correlation_id
                .as_ref()
                .map_or(0, |correlation_id| correlation_id.as_str().len()),
        )
        .saturating_add(payload.owned_heap_bytes())
}

/// Conservative heap charge for `serde_json::Value`. The root `Value` itself
/// is already inside `RawEnvelope`; this counts allocations below it.
fn json_owned_heap_bytes(value: &serde_json::Value) -> usize {
    // `serde_json::Map` is a BTreeMap in this workspace. One sparse node holds
    // fixed-capacity key/value arrays and edges; 1 KiB per object plus 128
    // bytes per populated entry safely covers those nodes, String/Value
    // headers, and allocator bookkeeping without depending on serialized JSON
    // length (which badly undercounts arrays of primitives and sparse maps).
    const OBJECT_BASE_BYTES: usize = 1_024;
    const OBJECT_ENTRY_BYTES: usize = 128;
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
        serde_json::Value::String(text) => text.len(),
        serde_json::Value::Array(items) => items.iter().map(json_owned_heap_bytes).fold(
            items
                .len()
                .saturating_mul(std::mem::size_of::<serde_json::Value>()),
            usize::saturating_add,
        ),
        serde_json::Value::Object(fields) if fields.is_empty() => 0,
        serde_json::Value::Object(fields) => fields.iter().fold(
            OBJECT_BASE_BYTES.saturating_add(fields.len().saturating_mul(OBJECT_ENTRY_BYTES)),
            |total, (key, value)| {
                total
                    .saturating_add(key.len())
                    .saturating_add(json_owned_heap_bytes(value))
            },
        ),
    }
}

impl<P> EventEnvelope<P> {
    /// Re-wrap with a different payload, preserving all envelope fields.
    pub fn map_payload<Q>(self, f: impl FnOnce(P) -> Q) -> EventEnvelope<Q> {
        EventEnvelope {
            schema_version: self.schema_version,
            event_id: self.event_id,
            seq: self.seq,
            session_id: self.session_id,
            branch_id: self.branch_id,
            run_id: self.run_id,
            agent_id: self.agent_id,
            device_id: self.device_id,
            authority_epoch: self.authority_epoch,
            worker_generation: self.worker_generation,
            causation_id: self.causation_id,
            correlation_id: self.correlation_id,
            committed_at_ms: self.committed_at_ms,
            render: self.render,
            payload: f(self.payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorPresentation;
    use crate::history::{NodeKind, TreeNode};
    use crate::ids::{ItemId, NodeId};
    use crate::item::ItemEvent;
    use crate::reply::{ReplyArenaWriter, ReplyText};
    use crate::verify::VerifyVerdict;

    fn reply_text(parts: &[&str]) -> ReplyText {
        let mut arena = ReplyArenaWriter::new();
        for part in parts {
            let _ = arena.append((*part).to_owned());
        }
        arena.seal()
    }

    fn reply_events(text: &ReplyText) -> Vec<(&'static str, EventPayload)> {
        let item_id = ItemId::new("item-1");
        vec![
            (
                "delta_text",
                EventPayload::Item(ItemEvent::Delta {
                    item_id: item_id.clone(),
                    delta: ItemDelta::Text { text: text.clone() },
                }),
            ),
            (
                "delta_reasoning",
                EventPayload::Item(ItemEvent::Delta {
                    item_id: item_id.clone(),
                    delta: ItemDelta::Reasoning { text: text.clone() },
                }),
            ),
            (
                "started_agent",
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: TurnItem::AgentMessage { text: text.clone() },
                }),
            ),
            (
                "completed_agent",
                EventPayload::Item(ItemEvent::Completed {
                    item_id: item_id.clone(),
                    item: TurnItem::AgentMessage { text: text.clone() },
                }),
            ),
            (
                "completed_incomplete_agent",
                EventPayload::Item(ItemEvent::Completed {
                    item_id: item_id.clone(),
                    item: TurnItem::IncompleteAgentMessage {
                        text: text.clone(),
                        interruption: ErrorPresentation::default(),
                    },
                }),
            ),
            (
                "completed_reasoning",
                EventPayload::Item(ItemEvent::Completed {
                    item_id,
                    item: TurnItem::Reasoning {
                        summary: text.clone(),
                    },
                }),
            ),
            (
                "assistant_node",
                EventPayload::NodeCommitted(TreeNode {
                    node: NodeId::new("node-1"),
                    parent: None,
                    kind: NodeKind::AssistantCommit {
                        text: text.clone(),
                        verdict: VerifyVerdict::NotApplicable,
                    },
                }),
            ),
        ]
    }

    fn reply_envelope(event: EventPayload) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("event-1"),
            seq: 7,
            session_id: SessionId::new("session-1"),
            branch_id: None,
            run_id: Some(RunId::new("run-1")),
            agent_id: None,
            device_id: DeviceId::new("device-1"),
            authority_epoch: 2,
            worker_generation: 3,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 4,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Verbatim,
            },
            payload: RawPayload::from_event(event).expect("reply payload"),
        }
    }

    #[test]
    fn chunked_json_and_messagepack_cover_every_reply_path_byte_for_byte() {
        let escaped = reply_text(&["left \"quote\"\\\n", "مرز ", "😀 right\u{0007}"]);
        for (path, event) in reply_events(&escaped) {
            let envelope = reply_envelope(event);
            assert!(
                envelope
                    .payload
                    .reply_text()
                    .is_some_and(|text| text.shares_arena_with(&escaped)),
                "{path} was flattened before serialization"
            );
            let expected_json = serde_json::to_vec(&envelope).expect("legacy JSON");
            let mut actual_json = Vec::new();
            write_envelope_json(&mut actual_json, &envelope).expect("chunked JSON");
            assert_eq!(actual_json, expected_json, "JSON path {path}");
            let replayed_json: RawEnvelope =
                serde_json::from_slice(&expected_json).expect("replay JSON envelope");
            assert!(
                replayed_json.payload.reply_text().is_some(),
                "JSON replay did not promote {path}"
            );
            let mut replayed_json_bytes = Vec::new();
            write_envelope_json(&mut replayed_json_bytes, &replayed_json)
                .expect("replayed chunked JSON");
            assert_eq!(
                replayed_json_bytes, expected_json,
                "JSON replay path {path}"
            );

            let expected_messagepack =
                rmp_serde::to_vec_named(&envelope).expect("legacy MessagePack");
            let mut actual_messagepack = Vec::new();
            write_envelope_messagepack(&mut actual_messagepack, &envelope)
                .expect("chunked MessagePack");
            assert_eq!(
                actual_messagepack, expected_messagepack,
                "MessagePack path {path}"
            );
            let replayed_messagepack: RawEnvelope =
                rmp_serde::from_slice(&expected_messagepack).expect("replay MessagePack envelope");
            assert!(
                replayed_messagepack.payload.reply_text().is_some(),
                "MessagePack replay did not promote {path}"
            );
            let mut replayed_messagepack_bytes = Vec::new();
            write_envelope_messagepack(&mut replayed_messagepack_bytes, &replayed_messagepack)
                .expect("replayed chunked MessagePack");
            assert_eq!(
                replayed_messagepack_bytes, expected_messagepack,
                "MessagePack replay path {path}"
            );
        }
    }

    #[test]
    fn every_reply_path_matches_at_all_messagepack_string_headers() {
        for len in [0, 1, 31, 32, 255, 256, 65_535, 65_536] {
            let first = "x".repeat(len / 2);
            let second = "x".repeat(len.saturating_sub(first.len()));
            let text = reply_text(&[&first, &second]);
            for (path, event) in reply_events(&text) {
                let envelope = reply_envelope(event);
                let expected = rmp_serde::to_vec_named(&envelope).expect("legacy MessagePack");
                let mut actual = Vec::new();
                write_envelope_messagepack(&mut actual, &envelope).expect("chunked MessagePack");
                assert_eq!(actual, expected, "path {path}, reply length {len}");
            }
        }
    }
}
