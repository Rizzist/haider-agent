//! Session tool discovery changes presentation, never the authorized catalog.

use super::*;
use std::collections::BTreeSet;

const CORE_TOOLS: &[&str] = &[
    "fs_read",
    "fs_glob",
    "fs_search",
    "fs_edit",
    "fs_write",
    "process_exec",
    "todo_write",
    "list_tools",
];
const DISCOVERY_ROW_CAP: usize = 8;

#[derive(Debug, Clone)]
pub(super) struct ToolExposure {
    promoted: BTreeSet<String>,
    current: Arc<[ToolDefinition]>,
    fallback: Option<Arc<[ToolDefinition]>>,
}

impl HarnessConfig {
    /// Enables the coding surface plus durable/configured discoveries. The
    /// already-installed pack remains the complete authorization ceiling.
    pub fn enable_tool_discovery(&mut self, promoted: Vec<String>) {
        self.tool_exposure = Some(ToolExposure {
            promoted: promoted.into_iter().collect(),
            current: self.shared_tool_definitions(),
            fallback: self.full_fallback_tool_definitions(),
        });
        self.enforce_advertised_tool_ceiling = true;
        self.rebuild_tool_exposure();
    }

    /// Called only after installing a new full provider selection. Retain
    /// names, but intersect them again with that selection and its grant.
    pub(super) fn refresh_tool_exposure(&mut self) {
        let current = self.shared_tool_definitions();
        let fallback = self.full_fallback_tool_definitions();
        if let Some(exposure) = &mut self.tool_exposure {
            exposure.current = current;
            exposure.fallback = fallback;
            self.rebuild_tool_exposure();
        }
    }

    fn full_fallback_tool_definitions(&self) -> Option<Arc<[ToolDefinition]>> {
        self.shared_provider_tool_fallback_tools
            .clone()
            .or_else(|| {
                (!self.provider_tool_fallback_tools.is_empty())
                    .then(|| self.provider_tool_fallback_tools.clone().into())
            })
    }

    /// The owned standalone path has no provider base from which to rebuild
    /// its catalog. Recover the full snapshot before it clears shared_tools;
    /// the public owned vector was emptied when exposure installed its view.
    pub(super) fn restore_owned_tool_exposure(&mut self) {
        if let Some(exposure) = &self.tool_exposure {
            self.tools = exposure.current.as_ref().to_vec();
        }
    }

    pub(super) fn note_tool_exposure_fallback(&mut self) {
        if let Some(exposure) = &mut self.tool_exposure
            && let Some(fallback) = exposure.fallback.take()
        {
            exposure.current = fallback;
        }
    }

    fn rebuild_tool_exposure(&mut self) {
        let Some(exposure) = &self.tool_exposure else {
            return;
        };
        // Filter in catalog order. Discovery order, duplicate calls and
        // set iteration can never reorder a stable advertised prefix.
        let select = |full: &[ToolDefinition]| -> Arc<[ToolDefinition]> {
            full.iter()
                .filter(|tool| {
                    CORE_TOOLS.contains(&tool.name.as_str())
                        || exposure.promoted.contains(&tool.name)
                })
                .cloned()
                .collect::<Vec<_>>()
                .into()
        };
        let current = select(&exposure.current);
        let digest = canonical_tool_definitions_digest(&current);
        self.tools.clear();
        self.shared_tools = Some(current);
        self.tool_pack_digest = Some(digest.clone());
        self.provider_tool_fallback_tools.clear();
        self.shared_provider_tool_fallback_tools = exposure.fallback.as_deref().map(select);
        self.provider_tool_fallback_digest = self
            .shared_provider_tool_fallback_tools
            .as_deref()
            .map(canonical_tool_definitions_digest);
        if let Some(boundaries) = &mut self.usage_scope.cache_boundaries {
            boundaries.tool_pack = digest;
        }
    }

    fn discovered_tool_result(&self, args: serde_json::Value) -> BoundedResult {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            #[serde(default)]
            filter: Option<String>,
        }
        let parsed = serde_json::from_value::<Request>(args)
            .map_err(|error| format!("invalid list_tools arguments: {error}"))
            .and_then(|request| {
                let filter = request.filter.map(|filter| filter.trim().to_owned());
                if filter.as_ref().is_some_and(|filter| filter.len() > 128) {
                    Err("list_tools filter must contain at most 128 bytes".to_owned())
                } else {
                    Ok(filter.filter(|filter| !filter.is_empty()))
                }
            });
        let mut result = BoundedResult {
            preview: String::new(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        };
        let filter = match parsed {
            Ok(filter) => filter,
            Err(reason) => {
                result.preview = serde_json::json!({"error": reason}).to_string();
                result.status = ToolResultStatus::Rejected;
                result.reason = Some(reason);
                return result;
            }
        };
        let catalog = self.tool_exposure.as_ref().map_or_else(
            || self.tool_definitions(),
            |exposure| exposure.current.as_ref(),
        );
        let Some(filter) = filter else {
            result.preview = serde_json::json!({
                "tools": catalog.iter().map(|tool| &tool.name).collect::<Vec<_>>(),
                "hint": "Call list_tools with a name or keyword filter to describe and enable matching tools for this session."
            }).to_string();
            return result;
        };
        let filter = filter.to_ascii_lowercase();
        let exact = catalog
            .iter()
            .any(|tool| tool.name.eq_ignore_ascii_case(&filter));
        let matches = catalog
            .iter()
            .filter(|tool| {
                if exact {
                    tool.name.eq_ignore_ascii_case(&filter)
                } else {
                    tool.name.to_ascii_lowercase().contains(&filter)
                        || tool.description.to_ascii_lowercase().contains(&filter)
                }
            })
            .collect::<Vec<_>>();
        let selected = matches
            .iter()
            .take(DISCOVERY_ROW_CAP)
            .copied()
            .collect::<Vec<_>>();
        let hint = if matches.is_empty() {
            "No authorized tool matched; call list_tools without a filter for names."
        } else {
            "These tools are now advertised for the rest of this session."
        };
        result.preview = serde_json::json!({"tools": selected, "hint": hint}).to_string();
        result.data = Some(haider_protocol::tool::ToolResultData::ToolsDiscovered {
            promoted: selected.iter().map(|tool| tool.name.clone()).collect(),
        });
        if matches.len() > DISCOVERY_ROW_CAP {
            let original = serde_json::json!({"tools": matches, "hint": hint}).to_string();
            result.declare_truncation(haider_protocol::tool::ToolTruncation::from_bytes(
                original.as_bytes(),
                0,
            ));
        }
        result
    }

    fn promote_committed_tools(&mut self, result: &BoundedResult) {
        if result.status != ToolResultStatus::Completed {
            return;
        }
        let Some(haider_protocol::tool::ToolResultData::ToolsDiscovered { promoted }) =
            &result.data
        else {
            return;
        };
        let Some(exposure) = &mut self.tool_exposure else {
            return;
        };
        let before = exposure.promoted.len();
        let allowed = promoted
            .iter()
            .filter(|name| exposure.current.iter().any(|tool| &tool.name == *name))
            .cloned()
            .collect::<Vec<_>>();
        exposure.promoted.extend(allowed);
        if before != exposure.promoted.len() {
            self.rebuild_tool_exposure();
            if let Some(compactor) = &self.context_compactor
                && let Some(updated) =
                    compactor.with_tool_definitions(self.shared_tool_definitions())
            {
                self.context_compactor = Some(updated);
            }
        }
    }
}

impl HarnessActor {
    pub(super) async fn complete_list_tools(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
    ) -> Result<Message, DriveError> {
        let args = parse_tool_args(&tools[index])?;
        let result =
            tools[index].correct_result(self.config.discovered_tool_result(args.as_ref().clone()));
        let call_id = tools[index].call_id.clone();
        self.commit_tool_result_and_completion(run_id, &tools[index], &result)
            .await?;
        // Publication cannot precede the durable receipt, including when
        // a store append fails or the daemon restarts at this boundary.
        self.config.promote_committed_tools(&result);
        let projection = model_tool_result_projection("list_tools", &result);
        tools.remove(index);
        Ok(Message::tool_result(
            call_id,
            projection.preview,
            projection.truncated,
        ))
    }
}

#[cfg(test)]
#[path = "tool_exposure_tests.rs"]
mod tests;
