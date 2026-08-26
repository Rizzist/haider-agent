use std::fmt;

use haider_protocol::cache::{ProviderViewBoundaryV1, ProviderViewLedgerV1};

use crate::TurnRequest;

pub const PROVIDER_VIEW_SERIALIZATION_VERSION: &str = "haider.provider-view.json.v1";

/// Adapter-prepared exact view. `previous_history_blocks` is reconstructed
/// from the current request through the preceding request's durable boundary;
/// it is validation scratch and is never persisted as a second copy.
#[derive(Debug, Clone)]
pub struct PreparedProviderView {
    ledger: ProviderViewLedgerV1,
    previous_history_blocks: Option<Vec<Vec<u8>>>,
}

impl PreparedProviderView {
    #[must_use]
    pub fn ledger(&self) -> &ProviderViewLedgerV1 {
        &self.ledger
    }
}

/// Result of exact prefix validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderViewContinuity {
    AppendOnly,
    DeclaredEpochChange,
}

/// A same-epoch old prefix could not be reproduced byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderViewInvariantError {
    MissingPreviousProjection,
    MiddleMutation {
        section: &'static str,
        block: Option<usize>,
    },
}

impl fmt::Display for ProviderViewInvariantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPreviousProjection => formatter.write_str(
                "provider-view prefix invariant could not reconstruct the preceding history boundary",
            ),
            Self::MiddleMutation { section, block } => {
                write!(formatter, "provider-view prefix invariant rejected an undeclared middle mutation in {section}")?;
                if let Some(block) = block {
                    write!(formatter, " block {block}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ProviderViewInvariantError {}

/// Byte-compares the exact old provider prefix before a same-epoch send.
/// Header, serialization, auth/reasoning, and compaction changes are already
/// content-addressed in the two epochs, so they are explicit cold boundaries
/// rather than accidental middle mutations.
pub fn validate_provider_view_prefix(
    previous: &ProviderViewLedgerV1,
    current: &PreparedProviderView,
) -> Result<ProviderViewContinuity, ProviderViewInvariantError> {
    let current_ledger = current.ledger();
    if previous.header_epoch != current_ledger.header_epoch
        || previous.cache_epoch != current_ledger.cache_epoch
        || previous.provider != current_ledger.provider
        || previous.model != current_ledger.model
        || previous.dialect != current_ledger.dialect
        || previous.serialization_version != current_ledger.serialization_version
        || previous.compaction_epoch != current_ledger.compaction_epoch
    {
        return Ok(ProviderViewContinuity::DeclaredEpochChange);
    }
    if previous.system_bytes != current_ledger.system_bytes {
        return Err(ProviderViewInvariantError::MiddleMutation {
            section: "system",
            block: None,
        });
    }
    if previous.tool_schema_bytes != current_ledger.tool_schema_bytes {
        return Err(ProviderViewInvariantError::MiddleMutation {
            section: "tools",
            block: None,
        });
    }
    let Some(reconstructed) = current.previous_history_blocks.as_ref() else {
        return Err(ProviderViewInvariantError::MissingPreviousProjection);
    };
    if previous.history_blocks.len() != reconstructed.len() {
        return Err(ProviderViewInvariantError::MiddleMutation {
            section: "history",
            block: Some(previous.history_blocks.len().min(reconstructed.len())),
        });
    }
    if let Some(block) = previous
        .history_blocks
        .iter()
        .zip(reconstructed)
        .position(|(expected, actual)| expected != actual)
    {
        return Err(ProviderViewInvariantError::MiddleMutation {
            section: "history",
            block: Some(block),
        });
    }
    Ok(ProviderViewContinuity::AppendOnly)
}

pub(crate) fn prepared_array_provider_view(
    request: &TurnRequest,
    prompt_payload: &serde_json::Value,
    dialect: &str,
    system_key: &str,
    tools_key: &str,
    history_key: &str,
    history_wire_start: usize,
    stable_wire_end: usize,
    previous_wire_end: Option<usize>,
    boundaries: Vec<ProviderViewBoundaryV1>,
) -> Option<PreparedProviderView> {
    let history = prompt_payload.get(history_key)?.as_array()?;
    let history_wire_start = history_wire_start.min(history.len());
    let stable_wire_end = stable_wire_end.max(history_wire_start).min(history.len());
    let history_blocks = serialize_values(&history[history_wire_start..stable_wire_end])?;
    let previous_history_blocks = match previous_wire_end {
        Some(end) => Some(serialize_values(
            &history[history_wire_start..end.max(history_wire_start).min(history.len())],
        )?),
        None => None,
    };
    prepared_serialized_provider_view(
        request,
        prompt_payload,
        dialect,
        system_key,
        tools_key,
        history_blocks,
        previous_history_blocks,
        boundaries,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepared_serialized_provider_view(
    request: &TurnRequest,
    prompt_payload: &serde_json::Value,
    dialect: &str,
    system_key: &str,
    tools_key: &str,
    history_blocks: Vec<Vec<u8>>,
    previous_history_blocks: Option<Vec<Vec<u8>>>,
    boundaries: Vec<ProviderViewBoundaryV1>,
) -> Option<PreparedProviderView> {
    let metadata = request.cache_metadata.as_ref()?;
    let system_bytes = serde_json::to_vec(&prompt_payload.get(system_key)).ok()?;
    let tool_schema_bytes = serde_json::to_vec(&prompt_payload.get(tools_key)).ok()?;
    let header_epoch = header_epoch(
        &metadata.provider,
        &request.model,
        dialect,
        &system_bytes,
        &tool_schema_bytes,
    );
    Some(PreparedProviderView {
        ledger: ProviderViewLedgerV1 {
            provider: metadata.provider.clone(),
            model: request.model.clone(),
            dialect: dialect.to_owned(),
            serialization_version: PROVIDER_VIEW_SERIALIZATION_VERSION.into(),
            header_epoch,
            cache_epoch: metadata.cache_epoch.clone(),
            compaction_epoch: metadata.compaction_epoch.clone(),
            reasoning_retention: format!(
                "append_only_provider_opaque_v1:{}",
                metadata.prefix_digests.reasoning_settings
            ),
            account_scope: metadata.account_scope.clone(),
            stable_history_end: u64::try_from(metadata.cacheable_history_end()).unwrap_or(u64::MAX),
            current_user_start: u64::try_from(metadata.current_user_start).unwrap_or(u64::MAX),
            latest_compaction_summary_end: metadata
                .latest_compaction_summary_end
                .map(|boundary| u64::try_from(boundary).unwrap_or(u64::MAX)),
            trim_sentinel: metadata.compaction_epoch.clone(),
            boundaries,
            system_bytes,
            tool_schema_bytes,
            history_blocks,
        },
        previous_history_blocks,
    })
}

fn serialize_values(values: &[serde_json::Value]) -> Option<Vec<Vec<u8>>> {
    values
        .iter()
        .map(|value| serde_json::to_vec(value).ok())
        .collect()
}

fn header_epoch(
    provider: &str,
    model: &str,
    dialect: &str,
    system_bytes: &[u8],
    tool_schema_bytes: &[u8],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.provider-header-epoch.v1\0");
    for component in [
        provider.as_bytes(),
        model.as_bytes(),
        dialect.as_bytes(),
        PROVIDER_VIEW_SERIALIZATION_VERSION.as_bytes(),
        system_bytes,
        tool_schema_bytes,
    ] {
        hasher.update(&(component.len() as u64).to_le_bytes());
        hasher.update(component);
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use haider_protocol::provider::PrefixDigests;

    use super::*;
    use crate::{Message, PromptCacheMetadata};

    fn request(messages: Vec<Message>, previous: Option<usize>) -> TurnRequest {
        TurnRequest {
            messages,
            model: "gpt-5.6".into(),
            max_tokens: 1,
            system_prompt: Some("system".into()),
            tools: Vec::new(),
            attachments: Vec::new(),
            cache_metadata: Some(PromptCacheMetadata {
                stable_history_end: 1,
                cacheable_history_end: None,
                current_user_start: 1,
                previous_stable_history_end: previous,
                latest_compaction_summary_end: None,
                prefix_digests: PrefixDigests {
                    reasoning_settings: "reasoning".into(),
                    ..PrefixDigests::default()
                },
                cache_epoch: "epoch".into(),
                header_epoch: String::new(),
                compaction_epoch: "root".into(),
                provider: "openai".into(),
                session_scope: "session".into(),
                account_scope: Some("account".into()),
                stable_prefix_tokens: 1_024,
                expected_later_reads: 2,
                reuse_gap_ms: None,
            }),
        }
    }

    #[test]
    fn same_epoch_middle_mutation_fails_closed() {
        let first_request = request(vec![Message::user_text("old")], None);
        let first_payload = serde_json::json!({
            "instructions": "system",
            "tools": [],
            "input": [{"role": "user", "content": "old"}],
        });
        let first = prepared_array_provider_view(
            &first_request,
            &first_payload,
            "openai_responses",
            "instructions",
            "tools",
            "input",
            0,
            1,
            None,
            Vec::new(),
        )
        .expect("first view");

        let grown_request = request(
            vec![Message::user_text("old"), Message::user_text("new")],
            Some(1),
        );
        let mutated_payload = serde_json::json!({
            "instructions": "system",
            "tools": [],
            "input": [
                {"role": "user", "content": "changed"},
                {"role": "user", "content": "new"},
            ],
        });
        let current = prepared_array_provider_view(
            &grown_request,
            &mutated_payload,
            "openai_responses",
            "instructions",
            "tools",
            "input",
            0,
            2,
            Some(1),
            Vec::new(),
        )
        .expect("current view");
        assert!(matches!(
            validate_provider_view_prefix(first.ledger(), &current),
            Err(ProviderViewInvariantError::MiddleMutation {
                section: "history",
                block: Some(0),
            })
        ));
    }

    #[test]
    fn append_only_growth_and_declared_header_change_are_allowed() {
        let first_request = request(vec![Message::user_text("old")], None);
        let first_payload = serde_json::json!({
            "instructions": "system",
            "tools": [],
            "input": [{"role": "user", "content": "old"}],
        });
        let first = prepared_array_provider_view(
            &first_request,
            &first_payload,
            "openai_responses",
            "instructions",
            "tools",
            "input",
            0,
            1,
            None,
            Vec::new(),
        )
        .expect("first view");
        let grown_request = request(
            vec![Message::user_text("old"), Message::user_text("new")],
            Some(1),
        );
        let grown_payload = serde_json::json!({
            "instructions": "system",
            "tools": [],
            "input": [
                {"role": "user", "content": "old"},
                {"role": "user", "content": "new"},
            ],
        });
        let grown = prepared_array_provider_view(
            &grown_request,
            &grown_payload,
            "openai_responses",
            "instructions",
            "tools",
            "input",
            0,
            2,
            Some(1),
            Vec::new(),
        )
        .expect("grown view");
        assert_eq!(
            validate_provider_view_prefix(first.ledger(), &grown),
            Ok(ProviderViewContinuity::AppendOnly)
        );

        let changed_payload = serde_json::json!({
            "instructions": "changed system",
            "tools": [],
            "input": [
                {"role": "user", "content": "old"},
                {"role": "user", "content": "new"},
            ],
        });
        let changed = prepared_array_provider_view(
            &grown_request,
            &changed_payload,
            "openai_responses",
            "instructions",
            "tools",
            "input",
            0,
            2,
            Some(1),
            Vec::new(),
        )
        .expect("changed view");
        assert_eq!(
            validate_provider_view_prefix(first.ledger(), &changed),
            Ok(ProviderViewContinuity::DeclaredEpochChange)
        );
    }
}
