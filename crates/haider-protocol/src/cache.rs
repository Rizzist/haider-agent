//! Cache-epoch policy and visible transition facts (CM3).

use crate::ids::SessionId;
use crate::item::TurnItem;
use crate::provider::CacheRequestDiagnosticV1;
use crate::reply::ReplyText;
use serde::{Deserialize, Serialize};
use std::io;
use std::io::Write as _;

/// Current exact provider-view ledger encoding. Older/future encodings remain
/// decodable for audit, but cannot authorize fork cache inheritance.
pub const PROVIDER_VIEW_SERIALIZATION_VERSION: &str = "haider.provider-view.json.v2";

/// Stable additive extension kind for a named cache-epoch transition.
pub const CACHE_EPOCH_TRANSITION_EXTENSION_KIND: &str = "cache_epoch_transition_v1";

/// Stable hidden extension kind written immediately before a physical
/// provider request. It preserves hashes even when opening or streaming the
/// request fails before the provider can report usage.
pub const CACHE_REQUEST_ATTEMPT_EXTENSION_KIND: &str = "cache_request_attempt_v1";

/// Stable hidden extension kind for the exact, provider-rendered cacheable
/// view written immediately before a physical provider request.
///
/// Unlike [`CACHE_REQUEST_ATTEMPT_EXTENSION_KIND`], this record names exact
/// serialized prompt blocks in the profile's content-addressed provider-view
/// store. It never embeds those bytes in the journal or resident session
/// state.
pub const PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND: &str = "provider_view_attempt_v1";

/// One explicit provider cache boundary selected by the placement planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderViewBoundaryV1 {
    /// Stable section name (`system`, `tools`, or `history`).
    pub section: String,
    /// Exclusive normalized-message boundary for history markers. System and
    /// tool markers omit it because they are not conversation messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_end: Option<u64>,
}

/// Content-addressed identity of one exact provider-rendered block.
///
/// The address is the ordinary `blake3:<hex>` CAS address. Keeping the exact
/// byte length beside it detects truncated/colliding index metadata without
/// loading the block into session state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderViewBlockRefV1 {
    pub content_hash: String,
    pub byte_len: u64,
}

impl ProviderViewBlockRefV1 {
    #[must_use]
    pub fn for_bytes(bytes: &[u8]) -> Self {
        Self {
            content_hash: format!("blake3:{}", blake3::hash(bytes).to_hex()),
            byte_len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        }
    }
}

/// Transient write payload handed from an adapter to the durable store.
///
/// This type is intentionally not serializable: bytes may exist while one
/// request is being prepared and persisted, but cannot enter an event or a
/// long-lived projection by accident.
#[derive(Debug)]
pub struct ProviderViewBlobV1 {
    pub block: ProviderViewBlockRefV1,
    bytes: ProviderViewBlobBytesV1,
    incrementally_hashed: bool,
}

#[derive(Debug)]
enum ProviderViewBlobBytesV1 {
    Contiguous(Vec<u8>),
    Segmented(Vec<ProviderViewBlobSegmentV1>),
}

/// One exact byte segment of a transient provider-view CAS object. Reply
/// segments retain the canonical arena range and are JSON-escaped only while
/// hashing or writing the object.
#[derive(Debug)]
pub enum ProviderViewBlobSegmentV1 {
    Bytes(Vec<u8>),
    JsonString(ReplyText),
}

impl ProviderViewBlobV1 {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            block: ProviderViewBlockRefV1::for_bytes(&bytes),
            bytes: ProviderViewBlobBytesV1::Contiguous(bytes),
            incrementally_hashed: false,
        }
    }

    /// Builds a content-addressed object without joining reply ranges into a
    /// second string or byte vector. A streamed reply must carry an
    /// incremental digest seeded before its first delta; ordinary prompt
    /// scalars without a delta-time hash retain the legacy bounded writer.
    pub fn segmented(segments: Vec<ProviderViewBlobSegmentV1>) -> io::Result<Self> {
        let bytes = ProviderViewBlobBytesV1::Segmented(segments);
        let incremental = incremental_block_for_provider_view_bytes(&bytes);
        let requires_incremental = match &bytes {
            ProviderViewBlobBytesV1::Segmented(segments) => segments.iter().any(|segment| {
                matches!(segment, ProviderViewBlobSegmentV1::JsonString(text) if text.has_incremental_json_views())
            }),
            ProviderViewBlobBytesV1::Contiguous(_) => false,
        };
        if requires_incremental && incremental.is_none() {
            return Err(io::Error::other(
                "streamed reply provider view lacks one exact incremental digest candidate",
            ));
        }
        let incrementally_hashed = incremental.is_some();
        let block = match incremental {
            Some(block) => block,
            None => block_for_provider_view_bytes(&bytes)?,
        };
        Ok(Self {
            block,
            bytes,
            incrementally_hashed,
        })
    }

    #[must_use]
    pub fn byte_len(&self) -> usize {
        usize::try_from(self.block.byte_len).unwrap_or(usize::MAX)
    }

    #[must_use]
    pub fn is_segmented(&self) -> bool {
        matches!(self.bytes, ProviderViewBlobBytesV1::Segmented(_))
    }

    #[must_use]
    pub fn is_incrementally_hashed(&self) -> bool {
        self.incrementally_hashed
    }

    /// Recomputes the address from the retained representation as a test and
    /// diagnostic oracle. Publication trusts the producer's incremental
    /// address and validates only the streamed byte count, avoiding a second
    /// full pass over the canonical reply.
    pub fn computed_block(&self) -> io::Result<ProviderViewBlockRefV1> {
        block_for_provider_view_bytes(&self.bytes)
    }

    pub fn write_to(&self, writer: &mut (impl io::Write + ?Sized)) -> io::Result<()> {
        match &self.bytes {
            ProviderViewBlobBytesV1::Contiguous(bytes) => writer.write_all(bytes),
            ProviderViewBlobBytesV1::Segmented(segments) => {
                for segment in segments {
                    match segment {
                        ProviderViewBlobSegmentV1::Bytes(bytes) => writer.write_all(bytes)?,
                        ProviderViewBlobSegmentV1::JsonString(text) => {
                            write_json_reply_scalar(writer, text)?;
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// Feeds the exact block bytes into a sink in bounded windows. This is
    /// used by provider prefix digests and never materializes a full reply.
    pub fn visit_bytes(&self, mut visit: impl FnMut(&[u8])) -> io::Result<()> {
        struct VisitorWriter<'a, F>(&'a mut F);
        impl<F: FnMut(&[u8])> io::Write for VisitorWriter<'_, F> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                (self.0)(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        self.write_to(&mut VisitorWriter(&mut visit))
    }
}

fn incremental_block_for_provider_view_bytes(
    bytes: &ProviderViewBlobBytesV1,
) -> Option<ProviderViewBlockRefV1> {
    let ProviderViewBlobBytesV1::Segmented(segments) = bytes else {
        return None;
    };
    let reply_index = segments
        .iter()
        .enumerate()
        .filter_map(|(index, segment)| {
            matches!(segment, ProviderViewBlobSegmentV1::JsonString(_)).then_some(index)
        })
        .collect::<Vec<_>>();
    let [reply_index] = reply_index.as_slice() else {
        return None;
    };
    let ProviderViewBlobSegmentV1::JsonString(text) = &segments[*reply_index] else {
        return None;
    };
    let mut prefix = Vec::new();
    for segment in &segments[..*reply_index] {
        let ProviderViewBlobSegmentV1::Bytes(bytes) = segment else {
            return None;
        };
        prefix.extend_from_slice(bytes);
    }
    let mut suffix = Vec::new();
    for segment in &segments[reply_index.saturating_add(1)..] {
        let ProviderViewBlobSegmentV1::Bytes(bytes) = segment else {
            return None;
        };
        suffix.extend_from_slice(bytes);
    }
    let (digest, byte_len) = text.incremental_json_view(&prefix, &suffix)?;
    Some(ProviderViewBlockRefV1 {
        content_hash: format!("blake3:{}", digest.to_hex()),
        byte_len,
    })
}

fn block_for_provider_view_bytes(
    bytes: &ProviderViewBlobBytesV1,
) -> io::Result<ProviderViewBlockRefV1> {
    struct HashWriter {
        hasher: blake3::Hasher,
        len: u64,
    }
    impl io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.hasher.update(bytes);
            self.len = self
                .len
                .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                .ok_or_else(|| io::Error::other("provider-view block length overflow"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut writer = HashWriter {
        hasher: blake3::Hasher::new(),
        len: 0,
    };
    match bytes {
        ProviderViewBlobBytesV1::Contiguous(bytes) => writer.write_all(bytes)?,
        ProviderViewBlobBytesV1::Segmented(segments) => {
            for segment in segments {
                match segment {
                    ProviderViewBlobSegmentV1::Bytes(bytes) => writer.write_all(bytes)?,
                    ProviderViewBlobSegmentV1::JsonString(text) => {
                        write_json_reply_scalar(&mut writer, text)?;
                    }
                }
            }
        }
    }
    Ok(ProviderViewBlockRefV1 {
        content_hash: format!("blake3:{}", writer.hasher.finalize().to_hex()),
        byte_len: writer.len,
    })
}

fn write_json_reply_scalar(
    writer: &mut (impl io::Write + ?Sized),
    text: &ReplyText,
) -> io::Result<()> {
    const WINDOW_BYTES: usize = 16 * 1_024;
    writer.write_all(b"\"")?;
    let mut result = Ok(());
    text.visit_strs(|segment| {
        let mut start = 0;
        while result.is_ok() && start < segment.len() {
            let mut end = start.saturating_add(WINDOW_BYTES).min(segment.len());
            while end > start && !segment.is_char_boundary(end) {
                end -= 1;
            }
            match serde_json::to_vec(&segment[start..end]) {
                Ok(encoded) => result = writer.write_all(&encoded[1..encoded.len() - 1]),
                Err(error) => result = Err(io::Error::other(error)),
            }
            start = end;
        }
    });
    result?;
    writer.write_all(b"\"")
}

/// Durable lookup cursor for the SQLite/CAS request view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderViewStorageV1 {
    pub session_id: SessionId,
    pub request_ordinal: u64,
    pub expires_at_ms: u64,
}

/// Exact immutable provider view associated with one cacheable request.
///
/// The three sections are serialized by the selected adapter using its
/// declared `serialization_version`, then stored in a disk-only CAS. History
/// is addressed block-by-block so an append-only reconstruction can compare
/// the exact old prefix by content address without retaining or re-encoding
/// durable data. The volatile newest tail is intentionally not included: it
/// is never eligible for a cache marker or this invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderViewLedgerV1 {
    pub provider: String,
    pub model: String,
    /// Exact output budget carried by the provider request. Although it sits
    /// after the reusable prompt on common wires, changing it is a material
    /// request-configuration boundary and must not inherit another route.
    #[serde(default)]
    pub max_tokens: u64,
    pub dialect: String,
    pub serialization_version: String,
    /// Content address of provider/model/system/tools/dialect/serialization.
    pub header_epoch: String,
    /// Full request cache domain, including auth/reasoning/compaction state.
    pub cache_epoch: String,
    pub compaction_epoch: String,
    /// The retention rule that shaped provider-owned reasoning blocks.
    pub reasoning_retention: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_scope: Option<String>,
    pub stable_history_end: u64,
    pub current_user_start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_compaction_summary_end: Option<u64>,
    /// Explicit epoch-changing trim sentinel. Root histories retain the root
    /// compaction epoch here rather than inventing a rotating trim window.
    pub trim_sentinel: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boundaries: Vec<ProviderViewBoundaryV1>,
    pub system_block: ProviderViewBlockRefV1,
    pub tool_schema_block: ProviderViewBlockRefV1,
    pub history_blocks: Vec<ProviderViewBlockRefV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<ProviderViewStorageV1>,
}

impl ProviderViewLedgerV1 {
    /// Domain-separated content address of this complete exact provider view.
    /// Fork inheritance persists this digest so the ledger remains the one
    /// authority for both byte comparison and inherited-segment identity.
    /// The physical storage cursor and expiry are excluded: moving the same
    /// immutable view to another request slot cannot change its prefix.
    pub fn prefix_digest(&self) -> Result<String, serde_json::Error> {
        #[derive(Serialize)]
        struct Prefix<'a> {
            provider: &'a str,
            model: &'a str,
            max_tokens: u64,
            dialect: &'a str,
            serialization_version: &'a str,
            header_epoch: &'a str,
            cache_epoch: &'a str,
            compaction_epoch: &'a str,
            reasoning_retention: &'a str,
            account_scope: &'a Option<String>,
            stable_history_end: u64,
            current_user_start: u64,
            latest_compaction_summary_end: Option<u64>,
            trim_sentinel: &'a str,
            boundaries: &'a [ProviderViewBoundaryV1],
            system_block: &'a ProviderViewBlockRefV1,
            tool_schema_block: &'a ProviderViewBlockRefV1,
            history_blocks: &'a [ProviderViewBlockRefV1],
        }

        let bytes = serde_json::to_vec(&Prefix {
            provider: &self.provider,
            model: &self.model,
            max_tokens: self.max_tokens,
            dialect: &self.dialect,
            serialization_version: &self.serialization_version,
            header_epoch: &self.header_epoch,
            cache_epoch: &self.cache_epoch,
            compaction_epoch: &self.compaction_epoch,
            reasoning_retention: &self.reasoning_retention,
            account_scope: &self.account_scope,
            stable_history_end: self.stable_history_end,
            current_user_start: self.current_user_start,
            latest_compaction_summary_end: self.latest_compaction_summary_end,
            trim_sentinel: &self.trim_sentinel,
            boundaries: &self.boundaries,
            system_block: &self.system_block,
            tool_schema_block: &self.tool_schema_block,
            history_blocks: &self.history_blocks,
        })?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"haider.session-fork.provider-prefix.v1\0");
        hasher.update(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(&bytes);
        Ok(hasher.finalize().to_hex().to_string())
    }
}

/// Dispatch-time wrapper which orders exact views alongside request usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderViewAttemptV1 {
    pub ordinal: u64,
    pub view: ProviderViewLedgerV1,
}

impl ProviderViewAttemptV1 {
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        Ok(TurnItem::Extension {
            kind: PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND.to_owned(),
            data: serde_json::to_value(self)?,
        })
    }

    /// Strict parser for the conversation store. A malformed known record is
    /// corruption and must not silently downgrade exact-prefix validation.
    pub fn try_from_extension_item(item: &TurnItem) -> Result<Option<Self>, serde_json::Error> {
        let TurnItem::Extension { kind, data } = item else {
            return Ok(None);
        };
        if kind != PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND {
            return Ok(None);
        }
        serde_json::from_value(data.clone()).map(Some)
    }
}

/// Hashes-and-counts-only evidence captured at provider dispatch time.
/// Response-local counters later join this record by `ordinal` through
/// [`crate::provider::RequestUsage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRequestAttemptV1 {
    pub ordinal: u64,
    pub diagnostic: CacheRequestDiagnosticV1,
}

impl CacheRequestAttemptV1 {
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        Ok(TurnItem::Extension {
            kind: CACHE_REQUEST_ATTEMPT_EXTENSION_KIND.to_owned(),
            data: serde_json::to_value(self)?,
        })
    }

    #[must_use]
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        (kind == CACHE_REQUEST_ATTEMPT_EXTENSION_KIND)
            .then(|| serde_json::from_value(data.clone()).ok())
            .flatten()
    }
}

/// Session policy for cache-destructive configuration changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicyMode {
    Economy,
    #[default]
    Balanced,
    Mobility,
}

/// Durable session cache policy. The balanced threshold is configurable and
/// defaults conservatively; callers may override it at session creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePolicySettingsV1 {
    #[serde(default)]
    pub mode: CachePolicyMode,
    #[serde(default = "default_cold_cost_threshold_microusd")]
    pub cold_cost_threshold_microusd: u64,
}

pub const fn default_cold_cost_threshold_microusd() -> u64 {
    50_000
}

impl Default for CachePolicySettingsV1 {
    fn default() -> Self {
        Self {
            mode: CachePolicyMode::Balanced,
            cold_cost_threshold_microusd: default_cold_cost_threshold_microusd(),
        }
    }
}

impl CachePolicySettingsV1 {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Named reason for a deliberate cache-domain change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheEpochTransitionReason {
    ConfigurationChanged,
    InstructionsChanged,
    ToolPackChanged,
    SystemVersionChanged,
    WebToolDegradation,
    Compaction,
}

/// Durable, UI-visible explanation for a cold boundary. It is operational
/// metadata only and must always ride `PromptRender::Omit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEpochTransitionV1 {
    pub reason: CacheEpochTransitionReason,
    /// Compaction is planned lifecycle work, never a failure/miss diagnosis.
    #[serde(default)]
    pub planned: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_fields: Vec<String>,
    #[serde(default)]
    pub invalidated_stable_tokens: u64,
    /// Present only for an API-key lane with known registry pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewarm_cost_usd: Option<f64>,
    /// Registry-derived base-input equivalents, useful even when a plan lane
    /// intentionally omits dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewarm_base_input_equivalent_tokens: Option<f64>,
    /// Stable identity for deduplicating a named component transition.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transition_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_cache_epoch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_cache_epoch: Option<String>,
}

impl CacheEpochTransitionV1 {
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        Ok(TurnItem::Extension {
            kind: CACHE_EPOCH_TRANSITION_EXTENSION_KIND.to_owned(),
            data: serde_json::to_value(self)?,
        })
    }

    #[must_use]
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        (kind == CACHE_EPOCH_TRANSITION_EXTENSION_KIND)
            .then(|| serde_json::from_value(data.clone()).ok())
            .flatten()
    }

    #[must_use]
    pub fn display_label(&self) -> String {
        let base = match self.reason {
            CacheEpochTransitionReason::ConfigurationChanged => "configuration changed",
            CacheEpochTransitionReason::InstructionsChanged => "instructions changed",
            CacheEpochTransitionReason::ToolPackChanged => "tool pack changed",
            CacheEpochTransitionReason::SystemVersionChanged => "system version changed",
            CacheEpochTransitionReason::WebToolDegradation => "web tool degraded",
            CacheEpochTransitionReason::Compaction => {
                "planned cache epoch transition; next turn history cold"
            }
        };
        if self.reason == CacheEpochTransitionReason::Compaction {
            return format!("· {base}");
        }
        let fields = if self.changed_fields.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.changed_fields.join(", "))
        };
        let mut label = format!(
            "· {base}{fields}; next turn cold — {} stable tokens invalidated",
            self.invalidated_stable_tokens
        );
        if let Some(cost) = self.rewarm_cost_usd {
            label.push_str(&format!(" · est ${cost:.4} re-warm"));
        } else if self.rewarm_base_input_equivalent_tokens.is_some() {
            label.push_str(" · plan");
        }
        label
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod incremental_provider_view_tests {
    use super::{ProviderViewBlobSegmentV1, ProviderViewBlobV1, ProviderViewBlockRefV1};
    use crate::reply::ReplyArenaWriter;

    #[test]
    fn per_delta_json_hash_matches_the_legacy_complete_canonical_bytes() {
        let prefix = br#"{"content":[{"text":"#;
        let suffix = br#","type":"output_text"}],"role":"assistant","type":"message"}"#;
        let tail = "\"quote\"\\\nمرز 😀";
        let mut writer =
            ReplyArenaWriter::new().with_incremental_json_view(prefix, b"deferred suffix");
        let _ = writer.append("left ".to_owned());
        let _ = writer.append(tail.to_owned());
        let text = writer.seal();
        let blob = ProviderViewBlobV1::segmented(vec![
            ProviderViewBlobSegmentV1::Bytes(prefix.to_vec()),
            ProviderViewBlobSegmentV1::JsonString(text),
            ProviderViewBlobSegmentV1::Bytes(suffix.to_vec()),
        ])
        .expect("segmented provider view");

        let legacy = serde_json::to_vec(&serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": format!("left {tail}")}],
        }))
        .expect("legacy complete JSON");
        assert!(blob.is_incrementally_hashed());
        assert_eq!(blob.block, ProviderViewBlockRefV1::for_bytes(&legacy));
        assert_eq!(
            blob.computed_block().expect("legacy complete hash"),
            blob.block
        );
    }

    #[test]
    fn streamed_reply_with_an_unseeded_prefix_cannot_fall_back_to_a_full_pass() {
        let mut writer = ReplyArenaWriter::new().with_standard_provider_json_views();
        let _ = writer.append("streamed native reply".to_owned());
        let text = writer.seal();
        let error = ProviderViewBlobV1::segmented(vec![
            ProviderViewBlobSegmentV1::Bytes(br#"{"signature":"late","thinking":"#.to_vec()),
            ProviderViewBlobSegmentV1::JsonString(text),
            ProviderViewBlobSegmentV1::Bytes(br#","type":"thinking"}"#.to_vec()),
        ])
        .expect_err("an unseeded streamed shape must fail closed");

        assert!(error.to_string().contains("incremental digest candidate"));
    }
}
