//! Logical wire-frame types.

use std::collections::BTreeSet;

use crate::command::{CommandCatalogItemWire, CommandDynamicSlotsWire, CommandInvokeOutcomeWire};
use haider_protocol::DeliveryMode;
use haider_protocol::agent::{AgentMessageReceipt, AgentMetricsSnapshot, AgentUsageMetrics};
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::graph::{GraphInspectSnapshot, GraphStatus as ConvergenceGraphStatus};
use haider_protocol::headless::HeadlessRunSpecV1;
use haider_protocol::ids::{
    AgentId, ArtifactRef, BranchId, CredentialAlias, EventId, GraphId, GraphRunSetId, ItemId,
    MenuId, NodeId, RunId, SessionId,
};
use haider_protocol::queue::QueueRow;
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::session_fork::{SessionMetaforkProposal, SessionMetaforkReviewManifest};
use haider_protocol::tool::{AttachmentBlock, ToolInventorySnapshot};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

/// The logical wire protocol encoded by this crate.
///
/// Decoding is deliberately strict about the top-level `"v"` field: any other
/// value is rejected, unlike unknown frame kinds, methods, and object fields,
/// which are tolerated. A version bump is a contract change; silent
/// cross-version decoding is not.
pub const WIRE_PROTOCOL_VERSION: u32 = 1;

/// Default v0.1 JSON body limit: 48 MiB. This admits one 32 MiB PDF after
/// base64 expansion plus request framing.
///
/// W3b advertises its actual configured value in [`Welcome::frame_limit`].
pub const DEFAULT_FRAME_LIMIT: usize = 48 * 1024 * 1024;

/// Maximum UTF-8 byte length of an opaque resident-binding correlator.
pub const RESIDENT_BINDING_TOKEN_MAX_BYTES: usize = 128;

/// Applies the wire's only resident-binding-token policy: a non-empty,
/// bounded ASCII token made from characters safe in argv/environment values.
/// The token remains opaque; no component may parse these characters into
/// structure or use the value for routing or authorization.
#[must_use]
pub fn resident_binding_token_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= RESIDENT_BINDING_TOKEN_MAX_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

/// Maximum decoded payload accepted by one `artifact.put` request.
///
/// The payload is base64 on the wire, so callers must also keep the encoded
/// request within the negotiated frame limit. This is deliberately one MiB
/// above the PDF lane's 32 MiB cap so the daemon can identify an uploaded
/// over-cap PDF with the PDF-specific typed rejection at turn admission.
pub const ARTIFACT_PUT_MAX_BYTES: usize = 33 * 1024 * 1024;

/// Maximum number of descendant nodes returned by one `session.fleet` read.
pub const FLEET_MAX_NODES: u32 = 512;
/// Defensive response-depth ceiling. Execution currently admits only three
/// delegation levels, but the read contract remains independently bounded.
pub const FLEET_MAX_DEPTH: u32 = 32;
/// Maximum number of descendant journals one live descendant attachment may
/// fan out at once. The request may negotiate any smaller positive bound.
pub const DESCENDANT_STREAM_MAX_CHILDREN: u32 = 64;

const fn default_frame_limit_u32() -> u32 {
    DEFAULT_FRAME_LIMIT as u32
}

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Constructs an opaque identifier.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Returns the opaque identifier text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(
    /// A connection-scoped, client-generated request identifier.
    RequestId
);
string_id!(
    /// An attachment created by `session.attach`.
    AttachmentId
);
string_id!(
    /// A durable, client-generated idempotency key.
    CommandId
);
string_id!(
    /// One daemon-instance-scoped OAuth browser flow.
    OAuthFlowId
);

/// Stable code for a request whose replay cursor is beyond the committed head.
pub const ERROR_CODE_CURSOR_AHEAD: &str = "cursor_ahead";
/// Stable code for a request forbidden by the connection's granted capabilities.
pub const ERROR_CODE_CAPABILITY_DENIED: &str = "capability_denied";
/// Stable code for a compare-and-set request that lost to an earlier resolution.
pub const ERROR_CODE_ALREADY_RESOLVED: &str = "already_resolved";
/// Stable code for a requested session, attachment, menu, or other resource not found.
pub const ERROR_CODE_NOT_FOUND: &str = "not_found";
/// Stable code for work rejected after the daemon entered its drain barrier.
pub const ERROR_CODE_DRAINING: &str = "draining";
/// Stable code for work refused because a daemon resource limit is already
/// reached — the connection admission cap is the first user (report §2.5).
/// Retrying later, after other work finishes, is the intended recovery.
pub const ERROR_CODE_OVERLOADED: &str = "overloaded";
/// Stable code for an opaque pagination cursor that cannot be decoded.
pub const ERROR_CODE_INVALID_CURSOR: &str = "invalid_cursor";
/// Stable code for a structurally invalid request: an unknown method or
/// attachment mode, a bad range/limit, or menu coordinates that do not match
/// the committed menu version.
pub const ERROR_CODE_INVALID_ARGUMENT: &str = "invalid_argument";
/// Stable code for a control command fenced by a newer worker generation.
pub const ERROR_CODE_STALE_GENERATION: &str = "stale_generation";
/// Stable code for a command that requires an active/nonterminal run.
pub const ERROR_CODE_RUN_NOT_ACTIVE: &str = "run_not_active";
/// Stable code for a session resource that is already occupied.
///
/// RESERVED in W3c1: golden-pinned per the report's R7 taxonomy but not yet
/// emitted — the daemon currently reports admission pressure (including
/// domain `Busy`) as the retryable [`ERROR_CODE_OVERLOADED`] family. The
/// W3c2 account actor is the intended first emitter; the review round owns
/// the busy-vs-overloaded mapping decision.
pub const ERROR_CODE_BUSY: &str = "busy";
/// Stable code for a provider-side turn failure.
///
/// First emitted by W3c2 login validation (R7): a retryable 429/529/5xx or
/// transport failure during credential validation reports this family with
/// `retryable: true`. Durable turn failures still surface as `RunFailed`
/// envelopes, not correlated responses (R3).
pub const ERROR_CODE_PROVIDER_ERROR: &str = "provider_error";
/// Stable retryable code for daemon infrastructure that prevented a provider
/// inventory lookup. This asserts no fact about whether the provider or its
/// models exist; clients may retry after refreshing daemon state.
pub const ERROR_CODE_PROVIDER_MODELS_UNKNOWN: &str = "provider_models_unknown";
/// Live provider inventories are refreshed after fifteen minutes. Clients
/// may display this policy, but the daemon remains the refresh authority.
pub const MODEL_INVENTORY_TTL_MS: u64 = 15 * 60 * 1_000;
/// Stable code for a credential that failed authentication (HTTP 401):
/// the key is invalid. Non-retryable.
pub const ERROR_CODE_UNAUTHORIZED: &str = "unauthorized";
/// Stable code for an authenticated identity that lacks permission for the
/// selected model/endpoint (HTTP 403). Non-retryable.
pub const ERROR_CODE_PERMISSION_DENIED: &str = "permission_denied";
/// Stable code for an operation that needs a credential no account provides.
pub const ERROR_CODE_CREDENTIAL_MISSING: &str = "credential_missing";
/// Stable code for a platform without a working secret vault (R10: the W3c
/// vault gate is macOS; non-macOS rejects login before staging/validation
/// with this code, never a generic internal message).
pub const ERROR_CODE_VAULT_UNSUPPORTED: &str = "vault_unsupported";
/// Stable code for a login retry whose staged secret no longer exists
/// (stage/pending-command TTL expiry, disconnect, or daemon restart): the
/// client must stage the secret again — an explicit recovery action, and
/// retryable once re-staged.
pub const ERROR_CODE_RESTAGE_REQUIRED: &str = "restage_required";
/// Stable code for a provider whose sanctioned OAuth registration is absent.
pub const ERROR_CODE_OAUTH_UNAVAILABLE: &str = "oauth_unavailable";
/// Stable code for an absent, expired, or differently-bound OAuth flow/ref.
pub const ERROR_CODE_OAUTH_FLOW_NOT_FOUND: &str = "oauth_flow_not_found";
/// Stable code for a management mutation fenced by a newer account/provider
/// snapshot. Retrying after refreshing that snapshot is the intended recovery.
pub const ERROR_CODE_REVISION_CONFLICT: &str = "revision_conflict";
/// Stable refusal for `provider.remove` when the named profile is not a
/// removable custom provider or credential descriptors still reference it.
pub const ERROR_CODE_PROVIDER_REMOVE_REFUSED: &str = "provider_remove_refused";
/// Stable rejection for a shell builtin whose durable daemon semantics are
/// deliberately not implemented by this protocol slice.
pub const ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN: &str = "unsupported_shell_builtin";
/// Stable rejection for an `artifact.put` payload above the decoded byte cap.
pub const ERROR_CODE_ARTIFACT_TOO_LARGE: &str = "artifact_too_large";
/// Stable rejection for a turn naming a CAS object that is absent or corrupt.
pub const ERROR_CODE_ATTACHMENT_NOT_FOUND: &str = "attachment_not_found";
/// Stable rejection for an image MIME outside the supported allowlist.
pub const ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED: &str = "attachment_mime_unsupported";
/// Stable rejection for one attachment above its per-object byte cap.
pub const ERROR_CODE_ATTACHMENT_TOO_LARGE: &str = "attachment_too_large";
/// Stable typed refusal for a PDF over its distinct byte cap.
pub const ERROR_CODE_PDF_TOO_LARGE: &str = "pdf_too_large";
/// Stable typed refusal for a PDF over the page-tree cap.
pub const ERROR_CODE_PDF_TOO_MANY_PAGES: &str = "pdf_too_many_pages";
/// Stable typed refusal for bytes that cannot be parsed as a PDF.
pub const ERROR_CODE_PDF_MALFORMED: &str = "pdf_malformed";
/// Stable rejection for more attachment blocks than one turn may carry.
pub const ERROR_CODE_TOO_MANY_ATTACHMENTS: &str = "too_many_attachments";
/// Stable rejection for attachment bytes above the per-turn aggregate cap.
pub const ERROR_CODE_ATTACHMENTS_TOO_LARGE: &str = "attachments_too_large";
/// Stable local refusal when an image is submitted to a non-vision provider.
pub const ERROR_CODE_VISION_UNSUPPORTED: &str = "vision_unsupported";
/// Stable refusal for a model selection whose implied provider is not
/// creatable on this daemon. Model selection is the user-facing act; the
/// provider is an attribute of the selected model row, and this code names
/// the one honest reason the row cannot be selected.
pub const ERROR_CODE_PROVIDER_UNAVAILABLE: &str = "provider_unavailable";
/// Stable refusal for a model selection naming a model outside the implied
/// provider's KNOWN discovered inventory. A provider without a discovered
/// inventory never produces this code — selection is accepted honestly and
/// provider errors surface at turn time.
pub const ERROR_CODE_MODEL_UNKNOWN: &str = "model_unknown";

/// A `session.select_effort` refusal (G3): the requested effort is not in
/// the CURRENT pair's declared ladder — including the empty-ladder case
/// where the pair declares no effort vocabulary at all.
pub const ERROR_CODE_EFFORT_UNSUPPORTED: &str = "effort_unsupported";

/// A `session.select_fast` refusal (G3): the CURRENT pair is not in the
/// static fast-mode gate. Turning fast OFF is always accepted.
pub const ERROR_CODE_FAST_UNSUPPORTED: &str = "fast_unsupported";
/// Stable refusal for input-mirroring text above its field-specific byte cap.
pub const ERROR_CODE_SURFACE_TEXT_TOO_LARGE: &str = "surface_text_too_large";
/// A cache-sensitive live-session change needs an explicit second-step
/// confirmation to create a fresh epoch.
pub const ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED: &str = "cache_epoch_confirmation_required";
pub const ERROR_CODE_GRAPH_ALREADY_ACTIVE: &str = "graph_already_active";
pub const ERROR_CODE_GRAPH_NOT_ACTIVE: &str = "graph_not_active";
pub const ERROR_CODE_GRAPH_WRONG_NODE: &str = "graph_wrong_node";
/// Undo/redo freshness refused to overwrite bytes not produced by the target.
pub const ERROR_CODE_CHECKPOINT_CONFLICT: &str = "checkpoint_conflict";
/// A checkpoint command addressed history owned by another branch.
pub const ERROR_CODE_CHECKPOINT_BRANCH_MISMATCH: &str = "checkpoint_branch_mismatch";

/// Daemon implements receipt-backed session creation and metadata.
pub const FEATURE_SESSION_MUTATION_V1: &str = "session_mutation_v1";
/// Daemon implements durable submit/cancel turn control.
pub const FEATURE_TURN_CONTROL_V1: &str = "turn_control_v1";
/// Daemon implements durable detached headless start/status/stop and replay
/// pins on the ordinary journal event stream.
pub const FEATURE_HEADLESS_RUN_V1: &str = "headless_run_v1";
/// Daemon enforces typed run-local token, cost, and wall-clock budgets.
pub const FEATURE_RUN_BUDGET_V1: &str = "run_budget_v1";
/// Daemon implements receipt-backed terminal-failure and backoff-wake retry
/// (`run.retry`).
pub const FEATURE_RUN_RETRY_V1: &str = "run_retry_v1";
/// Daemon implements durable idle-only context compaction.
pub const FEATURE_CONTEXT_COMPACTION_V1: &str = "context_compaction_v1";
/// Daemon can durably continue a failing turn on the next configured
/// provider/model lane after the current provider exhausts its accounts.
pub const FEATURE_FALLBACK_CHAIN_V1: &str = "fallback_chain_v1";
/// Daemon guards against ineffective repeated compaction and can durably
/// promote the session to a configured larger-context model.
pub const FEATURE_COMPACTION_GUARD_V1: &str = "compaction_guard_v1";
/// Daemon implements the durable `account.login_api` command (R7/R10).
pub const FEATURE_ACCOUNT_LOGIN_API_V1: &str = "account_login_api_v1";
/// Daemon implements connection-scoped `vault.stage` secret staging (R7).
pub const FEATURE_VAULT_STAGE_V1: &str = "vault_stage_v1";
/// Daemon implements loopback authorization-code/PKCE account flows.
pub const FEATURE_ACCOUNT_OAUTH_PKCE_V1: &str = "account_oauth_pkce_v1";
/// Daemon implements RFC 8628 device-code OAuth flows.
pub const FEATURE_ACCOUNT_OAUTH_DEVICE_V1: &str = "account_oauth_device_v1";
/// Daemon imports OAuth credentials from approved, daemon-local CLI stores.
pub const FEATURE_ACCOUNT_OAUTH_IMPORT_V1: &str = "account_oauth_import_v1";
/// Daemon publishes the approved OAuth import-source catalog and its
/// point-in-time credential-store availability.
pub const FEATURE_ACCOUNT_OAUTH_IMPORT_SOURCES_V1: &str = "account_oauth_import_sources_v1";
/// Daemon implements metadata-only device credential discovery and receipted
/// candidate import. There is no wire refresh action: same-alias re-login or
/// re-import replaces tokens, and broker-internal refresh stays daemon-owned.
pub const FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1: &str = "account_device_discovery_v1";
/// Daemon implements durable `account.add` for an OAuth-ready reference.
pub const FEATURE_ACCOUNT_MANAGEMENT_V1: &str = "account_management_v1";
/// Daemon serves `account.list_watch` and pushes `AccountsChanged` on every
/// published registry revision, so a client mirrors the account list without
/// polling.
pub const FEATURE_ACCOUNT_LIST_WATCH_V1: &str = "account_list_watch_v1";
/// Daemon serves `account.set_label` and carries `label` on credential
/// descriptors, so a surface can show an operator-chosen name instead of an
/// alias or a provider identity string.
pub const FEATURE_ACCOUNT_LABEL_V1: &str = "account_label_v1";
/// Daemon implements provider management reads.
pub const FEATURE_PROVIDER_MANAGEMENT_V1: &str = "provider_management_v1";
/// Daemon implements durable `provider.configure`.
pub const FEATURE_PROVIDER_CONFIGURE_V1: &str = "provider_configure_v1";
/// Daemon implements durable custom-provider removal.
pub const FEATURE_PROVIDER_REMOVE_V1: &str = "provider_remove_v1";
/// Daemon implements provider-owned model discovery refresh.
pub const FEATURE_PROVIDER_MODELS_V1: &str = "provider_models_v1";

/// Headless enumeration of every registered provider and its complete
/// published model inventory, composed from `provider.list` and
/// `account.list` without probing providers inline.
pub const FEATURE_MODELS_LIST_V1: &str = "models_list_v1";
/// Daemon implements live same-provider account rotation.
pub const FEATURE_ACCOUNT_ROTATION_V1: &str = "account_rotation_v1";
/// Daemon implements receipt-backed direct user shell execution.
pub const FEATURE_SHELL_EXEC_V1: &str = "shell_exec_v1";
/// Daemon commits direct shell provenance/output into later model context and
/// returns an immediate synthetic-run cancellation coordinate.
pub const FEATURE_USER_COMMAND_V1: &str = "user_command_v1";
/// Daemon implements the canonical read-only tool inventory snapshot.
pub const FEATURE_TOOL_INVENTORY_V1: &str = "tool_inventory_v1";
/// Daemon exposes the durable session-scoped monitor tool and source/delivery
/// runtime needed to wake a session from matching external events.
pub const FEATURE_MONITOR_V1: &str = "monitor_v1";
/// Daemon exposes typed client `monitor.list`, `monitor.register`, and
/// `monitor.remove` receipts. This is separate from the model-facing monitor
/// tool and from every private mobile transport convention.
pub const FEATURE_MONITOR_CONTROL_V1: &str = "monitor_control_v1";
/// Daemon exposes the cursor-replayable `monitor.watch` report stream.
/// Absence means there is no client delivery surface; clients must not infer
/// one from a private transport.
pub const FEATURE_MONITOR_DELIVERY_V1: &str = "monitor_delivery_v1";
/// Daemon persists and applies typed per-session write/exec permission overrides.
pub const FEATURE_SESSION_PERMISSION_OVERRIDES_V1: &str = "session_permission_overrides_v1";
/// Daemon persists and applies the explicit interactive/autonomous session mode.
pub const FEATURE_AUTONOMOUS_INTERACTION_V1: &str = "autonomous_interaction_v1";
/// Daemon implements the read-only, journal-derived session observation digest.
pub const FEATURE_SESSION_OBSERVE_V1: &str = "session_observe_v1";
/// Daemon accepts up to 64 observation coordinates in one read-only RPC.
pub const FEATURE_SESSION_OBSERVE_BATCH_V1: &str = "session_observe_batch_v1";
/// Daemon exposes the receipt-identical resident CLI submission alias.
pub const FEATURE_RESIDENT_TURN_SUBMIT_V1: &str = "resident_turn_submit_v1";
/// Daemon exposes typed crash-window state plus answerable recovery-card
/// coordinates through the existing observation/menu-answer surfaces.
pub const FEATURE_EFFECT_RECOVERY_V1: &str = "effect_recovery_v1";
/// Daemon implements the bounded, durable descendant-tree fleet snapshot.
pub const FEATURE_SESSION_FLEET_V1: &str = "session_fleet_v1";
/// Daemon implements the reconnectable, per-child-cursor descendant stream.
pub const FEATURE_SESSION_DESCENDANT_STREAM_V1: &str = "session_descendant_stream_v1";
/// The daemon serves receipt-backed named branch creation and branch-scoped turns.
pub const FEATURE_BRANCH_CREATE_V1: &str = "branch_create_v1";
/// Daemon serves receipt-backed session-level fork and review-gated metafork.
pub const FEATURE_SESSION_FORK_V1: &str = "session_fork_v1";
/// The daemon accepts receipt-free, content-addressed `artifact.put` uploads.
pub const FEATURE_ARTIFACT_PUT_V1: &str = "artifact_put_v1";
/// Daemon-owned hook discovery, execution, decision answers, and trust receipts.
pub const FEATURE_HOOKS_V1: &str = "hooks_v1";
/// Daemon supports trusted long-lived JSONL hook processes.
pub const FEATURE_HOOKS_SERVER_V1: &str = "hooks_server_v1";
/// Daemon implements owned direct-child messaging for tools and chip composers.
pub const FEATURE_AGENT_MESSAGE_V1: &str = "agent_message_v1";
/// Daemon implements receipted live-session model selection
/// (`session.select_model`), including cross-provider rows: the request's
/// optional `provider` names the selected model row's provider attribute,
/// and the next logical turn resolves through the committed pair.
pub const FEATURE_SESSION_MODEL_SELECT_V1: &str = "session_model_select_v1";
/// Daemon implements receipted live-session renaming (`session.rename`,
/// G2): the committed title lands in `sessions.meta_json`, a
/// `session_renamed` config fact is journaled atomically with the receipt,
/// and `session.list` summaries carry the title.
pub const FEATURE_SESSION_RENAME_V1: &str = "session_rename_v1";
/// Daemon implements the durable, shared per-session attention acknowledgement
/// (`session.seen`) and attention fields on session summaries.
pub const FEATURE_SESSION_SEEN_V1: &str = "session_seen_v1";
/// Daemon publishes the unified needs-input structure on session summaries:
/// EVERY parked-on-a-human menu (permission, recovery, update, secret, …)
/// presents one typed, secret-free, answerable card, so any surface can
/// resolve it through `menu.answer` without a terminal.
pub const FEATURE_SESSION_NEEDS_INPUT_V1: &str = "session_needs_input_v1";
/// Daemon owns the shared dynamic command catalog and the receipted/parked
/// command invocation door.
pub const FEATURE_COMMAND_DOOR_V1: &str = "command_door_v1";
/// Daemon implements receipted live-session effort selection
/// (`session.select_effort`), validated against the CURRENT pair's declared
/// effort ladder; `effort: null` reverts to the provider default (G3).
pub const FEATURE_SESSION_EFFORT_SELECT_V1: &str = "session_effort_select_v1";
/// Daemon implements the receipted live-session fast-mode toggle
/// (`session.select_fast`), statically gated to the pairs Anthropic
/// documents for the fast-mode research preview (G3).
pub const FEATURE_SESSION_FAST_SELECT_V1: &str = "session_fast_select_v1";

/// Headless read/write access to the durable per-session provider/model,
/// effort, and speed configuration through the existing observation and
/// receipted selection methods.
pub const FEATURE_SESSION_CONFIG_V1: &str = "session_config_v1";
/// Daemon vaults the profile transcription secret (the Deepgram API key)
/// and serves `transcription.secret_get`/`transcription.secret_set` on
/// authenticated same-UID local UDS connections only (T1).
pub const FEATURE_TRANSCRIPTION_V1: &str = "transcription_v1";
/// Daemon implements the read-only cross-provider `usage.report` snapshot:
/// per-account OAuth meters (normalized 0–1 utilization) plus journal-derived
/// local counters. Never carries secret material.
pub const FEATURE_USAGE_REPORT_V1: &str = "usage_report_v1";
/// Daemon serves device-local append-only usage history by UTC day and a
/// bounded, absence-preserving daily-total range.
pub const FEATURE_USAGE_HISTORY_V1: &str = "usage_history_v1";
/// Daemon serves revision-fenced held-message listing, removal, promotion to
/// steer, and revision-bearing deltas on the attached session event stream.
pub const FEATURE_QUEUE_CONTROL_V1: &str = "queue_control_v1";
/// Daemon publishes typed, provider-owned Haider Code plan/account status to
/// clients attached to sessions currently using the provider.
pub const FEATURE_HAIDER_CODE_PLAN_STATUS_V1: &str = "haider_code_plan_status_v1";
/// Daemon can open allow-listed macOS TCC panes for a durable in-session
/// computer permission card.
pub const FEATURE_COMPUTER_PERMISSION_ACTIONS_V1: &str = "computer_permission_actions_v1";
/// Daemon implements Convergence Graph M1 pin/evidence/status/abandon.
pub const FEATURE_CONVERGENCE_GRAPH_V1: &str = "convergence_graph_v1";
/// Daemon implements M2b general templates, dependency-ready sets, retained
/// graph instances, and receipted atomic `graph.switch`.
pub const FEATURE_CONVERGENCE_GRAPH_V2: &str = "convergence_graph_v2";
/// Daemon implements M2c finalization guardrails, rebuildable telemetry, and
/// the bounded `graph.inspect` read surface.
pub const FEATURE_CONVERGENCE_GRAPH_V3: &str = "convergence_graph_v3";
/// Daemon implements M2d todo run-sets, independently reduced child graphs,
/// aggregate telemetry, and receipted `graph.run_set.open`.
pub const FEATURE_CONVERGENCE_GRAPH_V4: &str = "convergence_graph_v4";
/// Daemon implements the Loom registry: agent types + pipe-source workflows
/// (`loom.list`, `loom.register_agent_type`, `loom.register_workflow`).
pub const FEATURE_LOOM_V1: &str = "loom_v1";
/// `loom.list` publishes the daemon-owned built-in + user workflow catalog,
/// including origin and whether each entry may be selected by a main session.
pub const FEATURE_WORKFLOW_CATALOG_V1: &str = "workflow_catalog_v1";
/// The Loom pipe compiler accepts the v0.0.961 dependency-DAG grammar:
/// explicit forks, multi-input joins, and conditional self/back edges.
pub const FEATURE_LOOM_PIPE_DAG_V1: &str = "loom_pipe_dag_v1";
/// Daemon supports prose-to-draft Loom authoring, typed text revision, and
/// confirmation into an immutable daemon-issued execution digest.
pub const FEATURE_LOOM_AUTHORING_V1: &str = "loom_authoring_v1";
/// Daemon exposes immutable built-in/user workflow-instance descriptors and
/// accepts template-digest fences on `graph.pin` and `graph.switch`.
pub const FEATURE_WORKFLOW_INSTANCE_V1: &str = "workflow_instance_v1";
/// W-flow — `loom.list` carries the declared-CLI device presence map.
pub const FEATURE_LOOM_CLI_PRESENCE_V1: &str = "loom_cli_presence_v1";
/// Typed-agent registrations create durable required-CLI install jobs whose
/// progress can be queried after disconnect or daemon restart.
pub const FEATURE_TYPED_AGENT_INSTALL_V1: &str = "typed_agent_install_v1";
/// Daemon returns install-job coordinates from typed registration and serves
/// typed failed-job retry plus cursor-replayable progress pages.
pub const FEATURE_TYPED_AGENT_INSTALL_CONTROL_V1: &str = "typed_agent_install_control_v1";
/// Session observe projections carry the active pinned workflow so a
/// client can render workflow state without issuing a separate graph.status
/// read (the workflow remains distinct from a child spawn's selector).
pub const FEATURE_SESSION_WORKFLOW_STATE_V1: &str = "session_workflow_state_v1";
/// W-flow — observation surfaces report the active run id (cancel coordinate).
pub const FEATURE_SESSION_RUN_ID_V1: &str = "session_run_id_v1";
/// Daemon can push changed/new session summaries after a read-only roster
/// watch is accepted.
pub const FEATURE_SESSION_LIST_WATCH_V1: &str = "session_list_watch_v1";
/// Daemon-owned volatile composer mirroring, watching, and input injection.
pub const FEATURE_INPUT_MIRROR_V1: &str = "input_mirror_v1";
/// Input mirrors carry metadata-only refs for ready composer attachments.
pub const FEATURE_INPUT_MIRROR_ATTACHMENTS_V1: &str = "input_mirror_attachments_v1";
/// Daemon-owned volatile status-segment publication and watching.
pub const FEATURE_STATUS_SEGMENT_V1: &str = "status_segment_v1";
/// Status mirrors carry structured state and detail beside their display line.
pub const FEATURE_STATUS_SEGMENT_STRUCTURED_V1: &str = "status_segment_structured_v1";
/// Daemon latches durable-store write health and pushes the transition —
/// degraded and recovered alike — to every connection as an out-of-band
/// `ProtocolError` (`store_full`/`store_read_only`/`store_unavailable`,
/// cleared by `store_healthy`), and replays the latched state to a client
/// connecting while degraded. Never journaled: the journal is the
/// component reporting that it cannot write.
pub const FEATURE_STORE_HEALTH_V1: &str = "store_health_v1";
/// The TUI of this release announces its attached session to the embedding
/// terminal — OSC 7791 `haider;attached=<session_id>`, empty payload back
/// at the launcher — on every binding change (attach, hop, detach). The bit
/// rides the daemon Welcome because TUI and daemon ship in lockstep; an
/// embedding ADE that sees it may trust the announce stream for PTY↔session
/// correlation instead of guessing.
pub const FEATURE_TUI_ATTACH_ANNOUNCE_V1: &str = "tui_attach_announce_v1";
/// Resident TUIs publish their foreground session as a typed RPC signal, and
/// the daemon fans that signal out to other connected clients. `None` is an
/// explicit unbind, while `worker_generation` fences announcements from a
/// superseded daemon worker generation. The OSC 7791 compatibility channel is
/// separate and remains advertised by [`FEATURE_TUI_ATTACH_ANNOUNCE_V1`].
pub const FEATURE_RESIDENT_SESSION_BINDING_V1: &str = "resident_session_binding_v1";
/// An accepted resident binding publication may carry an opaque
/// `binding_token`; the daemon stores it with that publisher and echoes it
/// verbatim on binding baselines and pushes. A tokenless publication remains
/// tokenless.
pub const FEATURE_RESIDENT_SESSION_BINDING_TOKEN_V1: &str = "resident_session_binding_token_v1";
/// Receipted per-session Loom agent-type binding (`session.select_agent_type`)
/// — the inline identity switch: a session takes a registered type's job
/// (volatile prompt tail, cache-epoch free) and accent until reverted.
pub const FEATURE_SESSION_AGENT_TYPE_SELECT_V1: &str = "session_agent_type_select_v1";
/// Session summaries carry typed lineage (`SessionSummary.kind` +
/// `parent_session_id`) reduced from the durable delegation record —
/// id-shape sniffing (`session-child-…`) is never the contract.
pub const FEATURE_SESSION_LINEAGE_V1: &str = "session_lineage_v1";
/// Daemon supports opt-in MessagePack encoding after the JSON handshake.
pub const FEATURE_WIRE_MSGPACK_V1: &str = "wire_msgpack_v1";
/// Daemon can omit superseded item deltas from the durable store phase of a
/// session attachment replay while preserving the replay cursor and live tail.
pub const FEATURE_SESSION_ATTACH_SEALED_V1: &str = "session_attach_sealed_v1";
/// ADE capability sniff: `haider export` renders seq-keyed rows (pipe/json
/// carry per-turn journal seq + a head_seq cursor, `--since` is exact).
pub const FEATURE_EXPORT_SEQ_V1: &str = "export_seq_v1";
/// Daemon maintains a rebuildable v2 JSONL sidecar for every session. Rows are
/// byte-identical to individually serialized unmasked JSON-export turns and
/// carry `(seq, ordinal)` identity plus an optional branch. Coverage lines
/// prove which non-projecting journal envelopes were inspected: readers set
/// `covered_through = max(row seqs, coverage values)` and are at head only
/// when it equals the roster/status `head_seq`. V2 readers must ignore unknown
/// row keys and unknown line kinds.
pub const FEATURE_PIPE_NATIVE_V2: &str = "pipe_native_v2";
/// Native-pipe tool rows carry a typed, unknown-tolerant lifecycle status;
/// clients never need to recover an outcome from presentation prose.
pub const FEATURE_PIPE_TOOL_STATUS_V1: &str = "pipe_tool_status_v1";
/// Event-sourced typed workflow activation state and cursor replay.
pub const FEATURE_WORKFLOW_GRAPH_V1: &str = "workflow_graph_v1";
/// Explicit queued/running typed-install cancellation. Kept separate from the
/// shipped retry/watch token so 962 negotiation retains its exact meaning.
pub const FEATURE_TYPED_AGENT_INSTALL_CANCEL_V1: &str = "typed_agent_install_cancel_v1";
/// Every client-visible Loom registry mutation carries a revision/digest CAS.
pub const FEATURE_LOOM_REGISTRY_CAS_V1: &str = "loom_registry_cas_v1";
/// Loom entries can be archived/unarchived without deleting retained content.
pub const FEATURE_LOOM_REGISTRY_ARCHIVE_V1: &str = "loom_registry_archive_v1";
/// Read-only author-document validation with the would-save canonical digest.
pub const FEATURE_LOOM_VALIDATION_V1: &str = "loom_validation_v1";
/// Replayable persist-before-publish Loom registry baselines and deltas.
pub const FEATURE_LOOM_REGISTRY_WATCH_V1: &str = "loom_registry_watch_v1";
/// Durable bounded workspace pre-images plus receipted undo/redo/rollback.
pub const FEATURE_CHECKPOINT_V1: &str = "checkpoint_v1";

/// Maximum UTF-8 bytes accepted for one mirrored input value or injected text.
pub const SURFACE_INPUT_MAX_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes accepted for one mirrored status line.
pub const SURFACE_STATUS_MAX_BYTES: usize = 4 * 1024;

/// One todo child returned by `graph.run_set.open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoGraphOpenedWire {
    pub todo_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on_todo_id: Option<u32>,
    pub child_graph_id: GraphId,
    pub attached_seq: u64,
    pub pinned_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_seq: Option<u64>,
}

/// Registry class of one immutable workflow instance. This is not session
/// lineage and does not grant execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkflowInstanceSourceV1 {
    BuiltIn,
    User,
    #[serde(other)]
    Unknown,
}

/// Exact daemon-owned bytes describing one pinnable workflow revision.
///
/// `digest` is the user-workflow content digest. Built-ins have no such
/// registry fact, so it is absent rather than copied from `template_digest`.
/// The template digest is the fence accepted by graph selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowInstanceV1 {
    pub id: String,
    pub revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub template_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipe_version: Option<String>,
    pub source: WorkflowInstanceSourceV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_metadata: Option<Vec<haider_protocol::loom::LoomNodeMeta>>,
    pub compiled_template: haider_protocol::graph::GraphTemplateSpec,
}

/// One daemon-owned workflow-catalog entry.
///
/// The `origin` tag is a registry classification, not execution authority.
/// Each known variant nests the complete record from its owning authority so
/// clients never have to reconstruct catalog facts from a summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkflowCatalogEntryV1 {
    BuiltIn {
        id: String,
        main_session_eligible: bool,
        template: haider_protocol::graph::GraphTemplateSpec,
    },
    User {
        id: String,
        main_session_eligible: bool,
        workflow: haider_protocol::loom::LoomWorkflow,
    },
    #[serde(other)]
    Unknown,
}

impl WorkflowCatalogEntryV1 {
    /// Uniform catalog identity. An unknown future origin has no v1 identity
    /// authority even if its raw object happened to contain an `id` field.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::BuiltIn { id, .. } | Self::User { id, .. } => Some(id),
            Self::Unknown => None,
        }
    }

    /// Whether the catalog authority permits this workflow class on a main
    /// session. This is eligibility only; it never grants execution.
    #[must_use]
    pub fn main_session_eligible(&self) -> Option<bool> {
        match self {
            Self::BuiltIn {
                main_session_eligible,
                ..
            }
            | Self::User {
                main_session_eligible,
                ..
            } => Some(*main_session_eligible),
            Self::Unknown => None,
        }
    }
}

/// Kind of client taking part in the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientKind {
    Cli,
    Tui,
    Gui,
    Headless,
    #[serde(other)]
    Unknown,
}

/// Connection capability requested or granted during negotiation.
///
/// The wire crate only models the `view | control` set; enforcing what a
/// capability permits is daemon (W3b) authorization policy, never codec logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    /// Receive session events.
    View,
    /// Additionally submit control commands such as [`WireFrame::MenuAnswer`].
    Control,
    /// Decode artifact for a capability this crate does not know. It is never
    /// granted by [`crate::negotiate`].
    #[serde(other)]
    Unknown,
}

/// A deterministically encoded set of capabilities.
pub type CapabilitySet = BTreeSet<Capability>;

/// Client handshake parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Lowest wire protocol version the client implements (inclusive).
    pub protocol_min: u32,
    /// Highest wire protocol version the client implements (inclusive).
    pub protocol_max: u32,
    /// Human-readable client product name, such as `haider-tui`.
    #[serde(default)]
    pub client_name: String,
    /// Client build/version string used for diagnostics and compatibility policy.
    #[serde(default)]
    pub client_version: String,
    /// Random identity for this client process instance.
    #[serde(default)]
    pub client_instance_id: String,
    pub client_kind: ClientKind,
    /// Ceiling for the grant: negotiation returns a subset of this set and
    /// never invents a capability the client did not ask for.
    #[serde(default)]
    pub capabilities_requested: CapabilitySet,
    /// Largest encoded body this client can receive.
    ///
    /// The daemon must not send a frame larger than the smaller of this value
    /// and its own configured limit. The default preserves decode tolerance
    /// for pre-release peers that omitted the additive field.
    #[serde(default = "default_frame_limit_u32")]
    pub max_receive_frame: u32,
    /// Preferred post-handshake wire encodings. Empty means JSON only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub encodings: Vec<String>,
}

/// Daemon lifecycle state advertised in [`Welcome`].
///
/// The wire crate only names the phases; their transitions and guarantees are
/// owned by W3b's recovery/drain machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LifecyclePhase {
    Starting,
    Recovering,
    Ready,
    Draining,
    Finalizing,
    Stopped,
    Failed,
    #[serde(other)]
    Unknown,
}

/// Server handshake response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The single wire protocol version selected by negotiation.
    pub protocol: u32,
    /// Random process-instance identity. W3b supplies its generation semantics.
    pub instance_id: String,
    /// Durable per-profile daemon generation. This is not a worker generation.
    pub daemon_generation: u64,
    /// Maximum JSON body bytes per frame on either transport. Both peers must
    /// enforce this limit before allocating a body buffer.
    pub frame_limit: u32,
    /// Durable profile identity served by this connection.
    #[serde(default)]
    pub profile_id: String,
    /// Daemon build/version string used for diagnostics and compatibility policy.
    #[serde(default)]
    pub daemon_version: String,
    pub lifecycle_phase: LifecyclePhase,
    /// Granted capability set: a subset of [`Hello::capabilities_requested`].
    #[serde(default)]
    pub capabilities_granted: CapabilitySet,
    /// Additive method families implemented by this daemon.
    ///
    /// Capabilities answer whether this connection may control the daemon;
    /// features answer whether the negotiated v1 peer implements a method.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub features: BTreeSet<String>,
    /// True only when `user_command_v1` was implemented but omitted because
    /// advertising it would exceed this peer's Welcome-frame limit.
    ///
    /// The short wire key is intentional: this marker replaces the sole
    /// withheld feature token on an already-tight frame and must make that
    /// frame smaller. False is never serialized.
    #[serde(rename = "uw", default, skip_serializing_if = "is_false")]
    pub user_command_withheld: bool,
    /// Selected post-handshake encoding. Absent means JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

/// A raw secret in transit on the sensitive same-UID UDS staging path (R7).
///
/// This type exists ONLY in the transport crate — domain `haider-protocol`
/// stays secret-free — and only inside [`RequestBody::VaultStage`], which the
/// daemon serves exclusively on an authenticated same-UID local UDS
/// connection. Laws:
///
/// - `Debug` is unconditionally redacted; ordinary frame formatting can
///   never reveal the value (test-pinned).
/// - The value is zeroized on drop, and both peers zeroize the encoded
///   frame buffers around it (`uds_codec::encode_zeroizing`, the daemon's
///   zeroizing decoder, the client's zeroizing writer).
/// - It must never be converted through a loggable `serde_json::Value`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretWire(String);

impl SecretWire {
    /// Wraps a raw secret for staging.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Grants access to the raw secret bytes; callers copy into their own
    /// zeroizing storage and drop this frame promptly.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Whether the staged secret is empty (invalid to stage).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretWire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretWire([REDACTED])")
    }
}

impl Drop for SecretWire {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.zeroize();
    }
}

/// A transient authorization URL whose query and state are secret-bearing.
///
/// This is intentionally not a `String`. Its normal formatting is redacted,
/// its allocation is zeroized on drop, and renderers must use the separately
/// returned provider origin + loopback port.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthAuthorizationWire {
    value: zeroize::Zeroizing<String>,
    provider_origin: String,
    loopback_port: Option<u16>,
}

impl OAuthAuthorizationWire {
    pub fn new(value: impl Into<String>) -> Self {
        Self::from_zeroizing(zeroize::Zeroizing::new(value.into()))
    }

    /// Moves an already-protected authorization URL into the wire value
    /// without creating a second ordinary secret-bearing allocation.
    pub fn from_zeroizing(value: zeroize::Zeroizing<String>) -> Self {
        let (provider_origin, loopback_port) = safe_authorization_display(&value);
        Self {
            value,
            provider_origin,
            loopback_port,
        }
    }

    /// Grants the browser-link boundary a short-lived view of the full URL.
    pub fn expose_authorization_url(&self) -> &str {
        self.value.as_str()
    }
}

impl std::fmt::Debug for OAuthAuthorizationWire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthAuthorizationWire")
            .field("provider_origin", &self.provider_origin)
            .field("loopback_port", &self.loopback_port)
            .finish()
    }
}

fn safe_authorization_display(value: &str) -> (String, Option<u16>) {
    let provider_origin = value
        .find("://")
        .and_then(|scheme_end| {
            let authority_start = scheme_end.checked_add(3)?;
            let authority_end = value[authority_start..]
                .find(['/', '?', '#'])
                .map_or(value.len(), |offset| authority_start + offset);
            (authority_end <= 512).then(|| value[..authority_end].to_owned())
        })
        .unwrap_or_else(|| "[REDACTED]".into());
    let redirect = value
        .find("redirect_uri=")
        .map(|start| &value[start + "redirect_uri=".len()..])
        .unwrap_or("");
    let marker = find_ascii_case_insensitive(redirect.as_bytes(), b"127.0.0.1%3a")
        .map(|index| index + b"127.0.0.1%3a".len())
        .or_else(|| {
            redirect
                .find("127.0.0.1:")
                .map(|index| index + "127.0.0.1:".len())
        });
    let loopback_port = marker.and_then(|start| {
        let digits = redirect[start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .collect::<Vec<_>>();
        std::str::from_utf8(&digits).ok()?.parse::<u16>().ok()
    });
    (provider_origin, loopback_port)
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

impl Serialize for OAuthAuthorizationWire {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.value.as_str())
    }
}

impl<'de> Deserialize<'de> for OAuthAuthorizationWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Single-use daemon-local claim reference for a verified token bundle.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthReadyRefWire(zeroize::Zeroizing<String>);

impl OAuthReadyRefWire {
    pub fn new(value: impl Into<String>) -> Self {
        Self(zeroize::Zeroizing::new(value.into()))
    }

    pub fn expose_reference(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for OAuthReadyRefWire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OAuthReadyRefWire([REDACTED])")
    }
}

impl Serialize for OAuthReadyRefWire {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for OAuthReadyRefWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Structured provider OAuth availability. An unavailable method always
/// carries a precise public reason and never allocates a listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthAvailabilityWire {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Machine-readable reason an OAuth import source is not currently
/// available. The paired human message remains authoritative for display;
/// `Unknown` lets an older client render that message for a newer code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthImportSourceUnavailableCodeWire {
    NotFound,
    Unreadable,
    #[serde(other)]
    Unknown,
}

/// Typed and displayable explanation for an unavailable OAuth import source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthImportSourceUnavailableReasonWire {
    pub code: OAuthImportSourceUnavailableCodeWire,
    pub message: String,
}

/// One daemon-owned OAuth import source and its point-in-time availability.
/// Internal environment-variable and filesystem-path details never cross the
/// wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthImportSourceWire {
    pub source: String,
    pub provider: String,
    pub default_alias: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<OAuthImportSourceUnavailableReasonWire>,
}

/// Provider adapter family. Unlike the frozen account enums, this enum is
/// tolerant from its first release so an older client can still display a
/// provider introduced by a newer daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderApiFamilyWire {
    AnthropicMessages,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    #[serde(rename = "gemini_generate_content")]
    GeminiGenerateContent,
    #[serde(other)]
    Unknown,
}

/// Immutable authentication requirement of a provider profile.
///
/// Custom providers may use API-key bearer authentication or no
/// authentication. OAuth is release-owned metadata and cannot be created by
/// an arbitrary endpoint configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderAuthRequirementWire {
    ApiKey,
    OAuth,
    None,
    #[serde(other)]
    Unknown,
}

/// Whether a configured provider is currently available for new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderAvailabilityWire {
    Available,
    Unavailable,
    #[serde(other)]
    Unknown,
}

/// Whether discovery may veto a model id for this provider.
///
/// Built-in adapters own authoritative inventories. User-configured,
/// OpenAI-compatible servers publish advisory inventories because routers and
/// local servers commonly omit otherwise valid passthrough ids from
/// `/v1/models`. Unknown preserves the conservative behavior for older
/// summaries and future provider kinds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModelInventoryAuthorityWire {
    Authoritative,
    Advisory,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Typed membership of one requested model in a provider's latest known
/// inventory. `Unlisted` is not `Available`: it is an honest advisory-catalog
/// miss that a custom compatible server may still accept on its chat wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModelInventoryStatusWire {
    Listed,
    Unlisted,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Whether a whole snapshot-producing subsystem was available for one read.
///
/// This is distinct from an empty successful snapshot: omitted means an old
/// daemon (therefore unknown), while `Available` plus an empty collection is
/// authoritative evidence that the collection is genuinely empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SnapshotAvailabilityWire {
    Available,
    Unavailable {
        reason: String,
    },
    #[serde(other)]
    Unknown,
}

/// Provider-declared metadata for one pickable model.
///
/// The G3 tuning fields are DAEMON truth: the daemon projects them from the
/// provider's own catalog, enriched from the pinned static capability tables
/// for providers whose catalog declares none (anthropic effort/fast, gemini
/// thinkingLevel). Clients hold no tables — an absent/empty field means "the
/// pair declares nothing" and tuning commands refuse honestly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDetailWire {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The pair's effort ladder, in the provider's own vocabulary and order.
    /// EMPTY (absent on the wire) means "no declared ladder".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_efforts: Vec<String>,
    /// The provider's declared default effort, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    /// Request speeds beyond standard the pair supports (`"fast"` today).
    /// EMPTY (absent on the wire) means standard only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_speeds: Vec<String>,
    /// Kimi's catalog-declared `supports_thinking_type` flag, carried so the
    /// provider factory can pick the documented wire shape (thinking.effort
    /// vs top-level reasoning_effort) without a client-side table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking_type: Option<bool>,
}

/// One provider's read-only management projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSummaryWire {
    pub provider: String,
    pub api_family: ProviderApiFamilyWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// OpenAI-family response-header wait for this profile. Absent selects
    /// the daemon's documented 60-second default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_open_timeout_ms: Option<u64>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub model_details: Vec<ModelDetailWire>,
    /// Unix time when the daemon last completed live discovery for this
    /// provider. Absent means the published rows are seeded/configured facts
    /// or no live inventory has been cached yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory_fetched_at_ms: Option<u64>,
    /// Whether a known inventory miss may veto inference. Absent on an older
    /// daemon means unknown, never advisory.
    #[serde(default, skip_serializing_if = "is_default")]
    pub inventory_authority: ModelInventoryAuthorityWire,
    #[serde(default)]
    pub auth_methods: Vec<haider_protocol::credential::AuthMethod>,
    pub availability: ProviderAvailabilityWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    pub enabled: bool,
}

impl ProviderSummaryWire {
    /// Classifies `model` without changing the advertised/pickable inventory.
    /// In particular, an unlisted custom passthrough id is never appended to
    /// `models` or fabricated as an available [`ModelDetailWire`] row.
    #[must_use]
    pub fn model_inventory_status(&self, model: &str) -> ModelInventoryStatusWire {
        if self.models.is_empty() {
            ModelInventoryStatusWire::Unknown
        } else if self.models.iter().any(|known| known == model) {
            ModelInventoryStatusWire::Listed
        } else {
            ModelInventoryStatusWire::Unlisted
        }
    }
}

/// Active account coordinate published beside `account.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderActiveWire {
    pub provider: String,
    pub alias: haider_protocol::ids::CredentialAlias,
}

/// Provider default-model coordinate published beside `account.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDefaultWire {
    pub provider: String,
    pub model: String,
}

/// Metadata-only projection of one first-party credential store found on the
/// daemon's device. Token, scope, client-secret, and device-id bytes have no
/// representation in this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCredentialCandidateWire {
    /// Opaque daemon-derived identifier consumed by account.import_device.
    pub candidate: String,
    /// Haider provider this credential would serve.
    pub provider: String,
    /// Human-facing first-party source name.
    pub source_label: String,
    /// Account email/label only when the probed store itself carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_label: Option<String>,
    /// Coarse fresh | expiring | expired | unknown access-token hint.
    pub freshness: String,
    /// Provider access-token expiry, when the store states one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Credential file inspected by the daemon. Never a token value.
    pub path: String,
    /// False when discovery is safe but reuse is unverified or unsupported.
    pub import_supported: bool,
    /// Honest, actionable explanation paired with an unsupported candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<String>,
}

/// Public-only flow progress. No variant can carry callback/token secrets or
/// a raw endpoint error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthFlowStatusWire {
    WaitingBrowser,
    WaitingDevice,
    Exchanging,
    Ready {
        oauth_reference: OAuthReadyRefWire,
        identity: String,
        expires_at_ms: u64,
    },
    Failed {
        public_code: String,
    },
    Expired,
    Cancelled,
    #[serde(other)]
    Unknown,
}

/// Tolerant method tag for `account.add`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AccountAddMethod {
    #[serde(rename = "oauth")]
    OAuth,
    ApiKey,
    MenuSecret,
    #[serde(other)]
    Unknown,
}

/// Why a secret is being staged (R7): the daemon validates the reference is
/// consumed by a matching operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StagePurpose {
    /// A provider API key headed for `account.login_api`.
    ApiKey,
    /// A provider-requested menu secret (`MenuInput::SecretVaultReference`).
    MenuSecret,
    /// Decode artifact for a purpose this crate does not know (tolerance
    /// discipline).
    #[serde(other)]
    Unknown,
}

/// Inclusive sequence range for a non-subscribing session read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqRange {
    /// Inclusive lower bound.
    pub start_seq: u64,
    /// Inclusive upper bound.
    pub end_seq: u64,
}

/// Requested attachment authority; mirrors [`Capability`] per attachment.
///
/// Whether the daemon honors the requested mode is authorization policy owned
/// by W3b, not by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttachMode {
    View,
    Control,
    #[serde(other)]
    Unknown,
}

/// Metadata returned when an attachment is established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachState {
    pub session_id: SessionId,
    /// Echo of the `after_seq` the client attached with: the greatest sequence
    /// it reported as fully applied (zero for complete history).
    pub requested_after_seq: u64,
    /// Committed head captured at attach time. Replay covers
    /// `(requested_after_seq, replay_through_seq]`; higher sequences are live.
    pub replay_through_seq: u64,
    /// Session/execution-scoped generation, distinct from daemon generation.
    pub worker_generation: u64,
    /// W3b fills this with the authority epoch observed at attachment time.
    pub authority_epoch: u64,
}

/// Cheap metadata returned by session listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    /// Greatest committed envelope sequence for the session.
    pub head_seq: u64,
    pub worker_generation: u64,
    /// Additive coarse run state at this committed head. `None` only from an
    /// older daemon; current list/watch producers always populate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_state: Option<ObserveRunStateWire>,
    /// W-flow (owner 2026-08-22) — identity of the run `run_state` describes,
    /// so a client can CANCEL what it is rendering.
    ///
    /// `turn.cancel` needs `run_id` + `worker_generation`, and until now no
    /// observation surface reported the id at all: it existed only on the
    /// acceptance reply of a client's OWN `turn.submit`, so a session started
    /// on another surface was uncancellable from anywhere else.
    ///
    /// Absent means NO ACTIVE RUN (or an older daemon) — never an error, and
    /// never a reason to render a stop control. Read it together with
    /// `run_state` from the SAME summary: pairing an id from one poll with a
    /// state from another can cancel a run that has already ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// Durable time at which any surface last acknowledged this session's
    /// activity. `None` means no acknowledgement has ever committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_at_ms: Option<u64>,
    /// Latest meaningful committed activity, reduced by the daemon. It is
    /// absent for a session with no user-relevant activity, never a client
    /// replay obligation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<u64>,
    /// Why the session currently needs a human. This is absent unless its
    /// daemon-owned run state is parked for permission, a question, or an
    /// approval.
    ///
    /// FROZEN at three kinds (v0.0.936 shipped the enum without a tolerance
    /// arm, so it must never grow) — superseded by [`Self::needs_input`],
    /// kept for 936 clients until they migrate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_why: Option<WaitingWhyWire>,
    /// v0.0.937 unified input-required contract: whenever this session is
    /// parked on ANY human input, the one typed, secret-free, answerable
    /// card — enough for a client to render it and resolve it through
    /// `menu.answer` (or the secret-reference input path) without ever
    /// reaching a terminal. Absent when the session needs nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_input: Option<NeedsInputWire>,
    /// Additive R2 field: typed configuration for live-created sessions.
    /// `None` for legacy `{}` rows and when an old daemon omits the field —
    /// readers must not infer anything from its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadataV1>,
    /// Additive roster-truth field: the provider paired with this session's
    /// model selection, sourced from the same committed metadata published in
    /// `metadata.provider`. `None` when an older daemon omits the field or the
    /// session has no typed metadata — readers must never infer a provider
    /// from its absence; absent means unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Additive roster-truth field: the model active at this committed
    /// session head. The latest durable `model_selected` fact wins, falling
    /// back to the create-time metadata model when no selection fact exists.
    /// `None` when an older daemon omits the field or the session has neither
    /// typed metadata nor a selection fact — readers must never infer a model
    /// from its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
    /// Lifetime cache-hit share over all logical input observed through this
    /// committed head. This includes the first request and each turn's new
    /// content, which could never have been cache hits, so it is not a cache
    /// health percentage and is mathematically unable to reach 100% for a
    /// nonempty session. A UI should label it as lifetime/all-input share and
    /// headline [`Self::cache_reread_hit_basis_points`] instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_lifetime_hit_basis_points: Option<u32>,
    /// Cache-hit rate over only input that could have been re-read from the
    /// preceding provider-call prefix. This answers "is the cache working?"
    /// and is the cache rate a UI should headline. For a current summary with
    /// usage, `None` means no input could yet have been re-read (for example,
    /// a first turn), not 0%; older daemons and sessions without usage truth
    /// also omit this additive field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_reread_hit_basis_points: Option<u32>,
    /// Additive canonical workspace coordinate for clients that list a
    /// session from a different process cwd. Absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_cwd: Option<String>,
    /// Additive roster-truth field: committed main-timeline user turns
    /// (durable `UserMessage` envelopes not scoped to a subagent), computed
    /// from the same sealed journal the observe surface replays. `None`
    /// only when an older daemon omits the field — readers must not infer
    /// emptiness from absence; `Some(0)` is reported exclusively for
    /// sessions with no committed user turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_count: Option<u64>,
    /// Additive roster-truth field: `used_tokens` of the latest durable
    /// [`ContextFootprint`] snapshot (the observe/W7 vocabulary). `Some(0)`
    /// is reported exclusively for truly empty sessions (no committed user
    /// turn and no snapshot); a session with content but no durable
    /// snapshot reports `None` — unknown is never rendered as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint_tokens: Option<u64>,
    /// Honesty marker paired with `footprint_tokens`: `Exact` when
    /// provider-reported usage supplied the count (or the session is truly
    /// empty — zero is exact), `Estimated` for locally accounted requests.
    /// Present exactly when `footprint_tokens` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint_truth: Option<ContextFootprintTruth>,
    /// Additive G2 field: the committed session title, so launcher rosters
    /// name rows without attaching. `None` for untitled sessions and when
    /// an older daemon omits the field — readers must not infer anything
    /// from its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Additive direct-agent metrics reduced through this committed head.
    /// Absent means an older daemon (or no reducible agent truth), never a
    /// zero-valued snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_metrics: Option<AgentMetricsSnapshot>,
    /// Additive lineage truth (`session_lineage_v1`): the delegating parent
    /// when this session is a durable delegation record's child. `None` for
    /// roots and from older daemons — `kind` is the discriminator, id-shape
    /// sniffing is never the contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    /// Additive typed kind (`session_lineage_v1`): `Some` from a
    /// lineage-aware daemon; `None` only from an older daemon — readers
    /// must not infer root from absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SessionKindWire>,
    /// Additive W-flow inline identity: the session's bound Loom agent-type
    /// id from committed metadata. `None` for plain sessions and from older
    /// daemons; accent surfaces join the loom snapshot's color by this id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Additive roster truth: the committed effort selection. `None` means
    /// provider default or an older daemon omitted the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Additive roster truth: the committed fast-mode selection. `Some(false)`
    /// is the real normal mode; `None` is reserved for older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<bool>,
    /// None until the per-session account seam lands; readers must not infer
    /// the daemon default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_alias: Option<String>,
}

/// Typed session lineage kind (`session_lineage_v1`), from the durable
/// delegation record — a subagent is a delegation's child session, a root
/// is any session no delegation names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKindWire {
    Root,
    Subagent,
}

/// Publisher-authored input value. Ownership is assigned by the daemon from
/// the authenticated connection and therefore has no request field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceInputPublishWire {
    pub text: String,
    /// Ready CAS attachment coordinates. Bytes never ride the volatile
    /// surface; the shape is the metadata-only hooks attachment payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<haider_protocol::hook::HookAttachmentMetadata>,
    pub revision: u64,
}

/// Publisher-authored status value. Ownership is assigned by the daemon from
/// the authenticated connection and therefore has no request field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceStatusPublishWire {
    pub line: String,
    /// Short lowercased status state for clients that do not render `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Human-readable state detail, when the typed TUI status carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub revision: u64,
}

/// Current daemon-owned volatile input snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceInputWire {
    pub text: String,
    /// Ready CAS attachment coordinates carried beside the authoritative text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<haider_protocol::hook::HookAttachmentMetadata>,
    pub revision: u64,
    pub owner: String,
}

/// Current daemon-owned volatile status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceStatusWire {
    pub line: String,
    /// Short lowercased status state carried beside the display line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Human-readable state detail carried beside the display line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub revision: u64,
    pub owner: String,
}

/// An operation routed to the current input owner. The daemon validates and
/// forwards it but never applies it to the mirrored buffer itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SurfaceInjectOp {
    Set {
        text: String,
    },
    Insert {
        text: String,
    },
    Clear,
    Submit,
    #[serde(other)]
    Unknown,
}

/// Result of a non-subscribing session read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReadResult {
    pub session_id: SessionId,
    pub range: SeqRange,
    pub head_seq: u64,
    /// Additive R2 field; same absence semantics as
    /// [`SessionSummary::metadata`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadataV1>,
    /// Latest durable request-local context snapshot at or before `head_seq`,
    /// independent of the requested envelope range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_context_footprint: Option<ContextFootprint>,
    #[serde(default)]
    pub envelopes: Vec<RawEnvelope>,
}

/// Stable coarse state used by non-interactive observation clients.
///
/// This intentionally does not expose every internal run phase. A newer
/// daemon value remains decodable by an older client as `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ObserveRunStateWire {
    Idle,
    Running,
    EffectUnknown,
    ParkedPermission,
    ParkedInput,
    Errored,
    Cancelled,
    #[serde(other)]
    Unknown,
}

/// Typed reason a session is currently parked for a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingWhyKindWire {
    Permission,
    Question,
    Approval,
}

/// Additive summary-level human-attention coordinate. The pending menu id is
/// intentionally optional so a parked run remains representable while a
/// legacy/recovery projection has no current menu identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingWhyWire {
    pub kind: WaitingWhyKindWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_menu_id: Option<MenuId>,
}

/// The typed reason a session is parked on a human (v0.0.937 unified
/// contract). Kinds mirror the daemon's menu vocabulary; `Unknown` absorbs
/// kinds a newer daemon may add, so this enum CAN grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedsInputKindWire {
    Permission,
    Question,
    Approval,
    Recovery,
    Secret,
    Update,
    TrustHook,
    Choice,
    Conflict,
    File,
    Exhausted,
    #[serde(other)]
    Unknown,
}

/// One unified, secret-free, ANSWERABLE input-required card. Everything a
/// client needs to render the park and resolve it: typed kind, display copy,
/// the exact `menu.answer` coordinates (menu id + request_seq +
/// worker_generation), and the option roster. `secret_answer` marks the one
/// kind whose answer must travel as a secret reference, never a plain value.
/// `since_ms` + the menu id give notification surfaces a stable identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsInputWire {
    pub kind: NeedsInputKindWire,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub safe_body: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu_id: Option<MenuId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ObserveMenuOptionWire>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub secret_answer: bool,
}

/// Secret-free projection of one currently answerable menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveMenuWire {
    pub kind: String,
    pub title: String,
    /// Additive durable menu identity. `None` only from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu_id: Option<MenuId>,
    /// Sequence of the committed `MenuOpened`; the menu-answer CAS fence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_seq: Option<u64>,
    /// Worker generation that opened the menu; distinct from the digest's
    /// current daemon generation after startup recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_generation: Option<u64>,
    /// Commit time of the durable `MenuOpened`; recovery fleet views use it
    /// as the exact parked-since timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ObserveMenuOptionWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<haider_protocol::error::ErrorPresentation>,
}

/// Secret-free display/answer coordinates for one durable menu choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveMenuOptionWire {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Additive (v0.0.937): the typed decision this choice commits for
    /// permission-style menus (`allow_once` / `allow_always` / `reject_once`
    /// / `reject_always`), so a client can style buttons without parsing
    /// labels. Absent for options with no permission semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
}

/// Daemon-persisted subagent identity and chip state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveSubagentWire {
    pub agent_id: haider_protocol::ids::AgentId,
    /// Only a callsign persisted by the daemon is exposed. Clients must not
    /// synthesize a TUI roster identity here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    pub task: String,
    pub state: String,
}

/// One read-only digest reduced from committed daemon truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionObserveDigest {
    pub session_id: SessionId,
    pub head_seq: u64,
    pub worker_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadataV1>,
    pub title: String,
    pub run_state: ObserveRunStateWire,
    /// Identity of the run `run_state` describes — same law as
    /// [`SessionSummary::run_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// `None` names the implicit main branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_branch_id: Option<BranchId>,
    /// Named branches. Main is implicit and is added by observation clients.
    #[serde(default)]
    pub branches: Vec<BranchDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_head_node_id: Option<NodeId>,
    pub main_head_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_context_footprint: Option<ContextFootprint>,
    #[serde(default)]
    pub pending_menus: Vec<ObserveMenuWire>,
    #[serde(default)]
    pub subagents: Vec<ObserveSubagentWire>,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub last_event_kinds: Vec<String>,
    /// Additive roster-truth field (v0.0.935 #13): committed main-timeline
    /// user turns at `head_seq`, from the same sealed-journal truth the
    /// session listing reports. `None` from older daemons and in
    /// metadata-only responses — readers must not infer zero from absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_count: Option<u64>,
    /// Additive roster-truth field (v0.0.935 #13): per-agent metrics rollup
    /// at `head_seq`, same source as the session listing. `None` from older
    /// daemons and in metadata-only responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_metrics: Option<AgentMetricsSnapshot>,
    /// v0.0.937 unified input-required card (same producer as the session
    /// listing's field): present whenever this session is parked on a human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_input: Option<NeedsInputWire>,
    /// Active per-session workflow for composer/status-strip consumers.
    /// This is native selection state for this session, not the
    /// `spawn_subagent.workflow` child-creation argument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<ConvergenceGraphStatus>,
}

/// Stable display state for one descendant in a fleet snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FleetAgentStateWire {
    Queued,
    Live,
    Waiting,
    Done,
    Failed,
    Cancelled,
    #[serde(other)]
    Unknown,
}

/// One recursively nested durable descendant. Metrics are direct/exclusive
/// for this child; consumers must not add snapshots from different heads for
/// the same agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetNodeWire {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    /// Persisted display identity only; clients may choose their own fallback
    /// when a callsign has not yet been assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    pub task: String,
    /// Absolute delegation depth from durable relation truth.
    pub depth: u32,
    pub parent_session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<AgentId>,
    pub state: FleetAgentStateWire,
    /// The v0.0.902 direct-agent snapshot. Elapsed time is
    /// `(terminal_at_ms | snapshot.generated_at_ms) - started_at_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<AgentMetricsSnapshot>,
    /// Exact number of this node's direct durable children omitted by the
    /// snapshot bounds. Zero means the empty `children` list is a real leaf.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub folded_children: u32,
    #[serde(default)]
    pub children: Vec<FleetNodeWire>,
}

/// Per-state counts over the nodes actually returned in a fleet snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStateCountsWire {
    pub queued: u32,
    pub live: u32,
    pub waiting: u32,
    pub done: u32,
    pub failed: u32,
    pub cancelled: u32,
}

/// Saturating totals over direct metrics for the returned nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMetricsTotalsWire {
    /// Sum of every returned node's direct elapsed duration at the snapshot's
    /// single `generated_at_ms` instant.
    pub elapsed_ms: u64,
    pub tool_attempts: u64,
    /// Absent when any returned node lacks durable usage truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsageMetrics>,
}

/// Daemon-side rollup. `complete` is false when the tree was bounded; all
/// values still describe exactly the nodes present in `roots`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRollupWire {
    pub node_count: u32,
    pub states: FleetStateCountsWire,
    pub max_depth: u32,
    pub metrics: FleetMetricsTotalsWire,
    /// False when one or more returned nodes had no reducible direct metrics
    /// or no durable usage truth for its token/cost totals.
    pub metrics_complete: bool,
    pub complete: bool,
}

/// Bounded, receipt-free descendant-tree snapshot for one durable session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFleetSnapshot {
    pub session_id: SessionId,
    pub generated_at_ms: u64,
    pub node_limit: u32,
    pub depth_limit: u32,
    #[serde(default)]
    pub roots: Vec<FleetNodeWire>,
    pub rollup: FleetRollupWire,
    pub truncated: bool,
}

/// One reconnect cursor for a descendant journal. Both lineage identities
/// are required so a client cannot accidentally apply one child's sequence
/// coordinate to another child session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescendantReplayCursorWire {
    pub session_id: SessionId,
    pub agent_id: AgentId,
    /// Greatest child-journal sequence the client has fully applied.
    pub after_seq: u64,
}

/// One descendant identity without any sequence claim. Used when the daemon
/// must request reconnect after purging frames it had admitted but cannot
/// know the client applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescendantIdentityWire {
    pub session_id: SessionId,
    pub agent_id: AgentId,
}

/// Negotiated bound for one descendant stream attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescendantFanoutWire {
    pub requested_children: u32,
    pub accepted_children: u32,
    pub hard_limit: u32,
}

/// Explicit accounting for descendants omitted by the negotiated fan-out or
/// the defensive lineage scan. `omitted_children` is exact only when
/// `count_complete` is true; otherwise it is a lower bound, never a claim
/// that the omitted set is empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescendantTruncationWire {
    pub truncated: bool,
    pub streamed_children: u32,
    pub omitted_children: u32,
    pub count_complete: bool,
}

/// Durable parent-turn anchors for one child. Every optional field preserves
/// absence from the parent journal; no sequence is inferred from neighboring
/// events or from the delegation row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescendantParentAnchorsWire {
    /// Parent `AgentSpawned` fact for this exact `agent_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_seq: Option<u64>,
    /// Parent completed `ChildSpawn` item for this exact `agent_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_item_seq: Option<u64>,
    /// Parent `AgentReport` fact for this exact `agent_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_seq: Option<u64>,
    /// Parent completed `ChildResult` item for this exact `agent_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_item_seq: Option<u64>,
}

/// One child in a descendant attachment baseline or lineage delta. The
/// session, agent, child-run, and parent-run identities remain independent
/// coordinates; none is derived from another's string shape. `children` is
/// populated only while nesting the baseline; delta consumers upsert the
/// named node and preserve its independently delivered child edges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DescendantStreamNodeWire {
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub child_run_id: RunId,
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<AgentId>,
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    pub task: String,
    pub state: FleetAgentStateWire,
    /// Echo of the reconnect cursor accepted for this child.
    pub requested_after_seq: u64,
    /// Sealed child-journal head for this baseline/delta. Replay covers
    /// `(requested_after_seq, replay_through_seq]` before `ChildCaughtUp`.
    pub replay_through_seq: u64,
    pub parent_anchors: DescendantParentAnchorsWire,
    /// Nested descendants in a baseline. Delta events leave this empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<DescendantStreamNodeWire>,
}

/// Reconnectable baseline returned before any descendant stream frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionDescendantBaselineWire {
    pub session_id: SessionId,
    pub generated_at_ms: u64,
    pub fanout: DescendantFanoutWire,
    pub truncation: DescendantTruncationWire,
    #[serde(default)]
    pub roots: Vec<DescendantStreamNodeWire>,
}

/// Typed lineage/state transition after the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DescendantChangeKindWire {
    Appeared,
    Updated,
    Terminated,
    #[serde(other)]
    Unknown,
}

/// Frames carried by a live descendant attachment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionDescendantStreamEventWire {
    /// A child appeared or its durable lineage/state/anchor projection
    /// changed. `Terminated` is emitted for the first transition into a
    /// terminal fleet state. This is a node upsert, not subtree replacement.
    Delta {
        change: DescendantChangeKindWire,
        child: DescendantStreamNodeWire,
    },
    /// One raw child-journal envelope. The outer tags are mandatory even
    /// though the raw envelope itself also carries its session coordinate.
    Envelope {
        session_id: SessionId,
        agent_id: AgentId,
        envelope: RawEnvelope,
    },
    /// Replay for one child is complete through this sealed high-water mark.
    ChildCaughtUp {
        session_id: SessionId,
        agent_id: AgentId,
        high_water_seq: u64,
    },
    /// A non-contiguous store page was observed. The stream never advances
    /// past this hole; `resume_after_seq` is diagnostic delivery position,
    /// while the client reconnects from its own applied cursor.
    RepairRequired {
        session_id: SessionId,
        agent_id: AgentId,
        resume_after_seq: u64,
        expected_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_seq: Option<u64>,
    },
    /// Updated explicit omission accounting after live lineage growth.
    Truncation {
        truncation: DescendantTruncationWire,
    },
    /// A future additive event subtype. It conveys no cursor or lineage
    /// authority to this decoder.
    #[serde(other)]
    Unknown,
}

/// Secret-free projection of one effective hook definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSummaryWire {
    pub name: String,
    pub digest: String,
    pub source: String,
    pub kind: String,
    pub event: String,
    pub trusted: bool,
    /// Additive daemon-owned classification. `None` means an older daemon;
    /// consumers may fall back only to the legacy `trusted` boolean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_state: Option<HookTrustStateWire>,
    pub decision: bool,
    pub timeout_ms: u64,
}

/// Daemon truth for a discovered hook's digest-pinned trust state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTrustStateWire {
    Trusted,
    Untrusted,
    RevokedByEdit,
}

/// Source families understood by the monitor registry. Availability is
/// reported separately because a typed source is not necessarily active on
/// this daemon/platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorSourceKindWire {
    Sms,
    Process,
    File,
    Poll,
    Timer,
    #[serde(other)]
    Unknown,
}

/// Complete typed monitor source declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorSourceWire {
    Sms,
    Process {
        command: String,
    },
    File {
        path: String,
    },
    Poll {
        command: String,
        interval_ms: u64,
    },
    Timer {
        interval_ms: u64,
    },
    #[serde(other)]
    Unknown,
}

/// Field selected by one monitor predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorFilterFieldWire {
    Address,
    Body,
    Payload,
    #[serde(other)]
    Unknown,
}

/// Comparison performed by one monitor predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorFilterOperatorWire {
    Equals,
    Contains,
    StartsWith,
    EndsWith,
    #[serde(other)]
    Unknown,
}

/// Optional source-local predicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorFilterWire {
    pub field: MonitorFilterFieldWire,
    pub operator: MonitorFilterOperatorWire,
    pub value: String,
    #[serde(default)]
    pub case_sensitive: bool,
}

fn monitor_default_report() -> bool {
    true
}

/// Action retained with a registration and copied verbatim to deliveries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorActionWire {
    #[serde(default = "monitor_default_report")]
    pub report: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<String>,
}

/// Whether a matching registration stops after its first occurrence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorOccurrenceWire {
    Once,
    #[default]
    Every,
    #[serde(other)]
    Unknown,
}

/// Registration lifetime. Timeout bounds are validated by the canonical
/// monitor parser in `haider-tools`, not duplicated in this wire crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorLifetimeWire {
    #[default]
    Session,
    Timeout {
        timeout_ms: u64,
    },
    #[serde(other)]
    Unknown,
}

/// Why a known source declaration cannot currently be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorSourceUnavailableReasonWire {
    AdapterInactive,
    #[serde(other)]
    Unknown,
}

/// Honest source-adapter state for this daemon/platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorSourceAvailabilityStateWire {
    Available,
    Unavailable {
        reason: MonitorSourceUnavailableReasonWire,
    },
    #[serde(other)]
    Unknown,
}

/// One row of the exhaustive sms/process/file/poll/timer availability table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorSourceAvailabilityWire {
    pub source: MonitorSourceKindWire,
    pub availability: MonitorSourceAvailabilityStateWire,
}

/// Capability policy returned with every monitor receipt. Control implies
/// View under daemon authorization, as on the rest of the client surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorControlPolicyWire {
    pub list: Capability,
    pub register: Capability,
    pub register_requires_control_attachment: bool,
    pub remove: Capability,
    pub remove_requires_control_attachment: bool,
    pub watch: Capability,
}

/// Client-visible durable registry row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorRegistrationWire {
    pub monitor_id: String,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    pub source: MonitorSourceWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<MonitorFilterWire>,
    pub action: MonitorActionWire,
    pub occurrence: MonitorOccurrenceWire,
    pub created_at_ms: u64,
    /// Source-hub watermark captured by registration. This is not a delivery
    /// replay cursor and clients must not use it as one.
    pub start_source_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// Structured refusal shared by the typed monitor control receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorControlRejectionWire {
    CapabilityDenied {
        required: Capability,
    },
    ControlAttachmentRequired,
    SourceUnavailable {
        source: MonitorSourceKindWire,
    },
    LimitReached {
        count: u32,
        limit: u32,
    },
    NotFound {
        monitor_id: String,
    },
    SessionNotFound,
    StaleGeneration {
        requested: u64,
        current: u64,
    },
    CursorAhead {
        requested: u64,
        head: u64,
    },
    InvalidRequest {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field: Option<String>,
        detail: String,
    },
    CommandConflict,
    ServiceStopped,
    StoreUnavailable {
        retryable: bool,
        detail: String,
    },
    #[serde(other)]
    Unknown,
}

/// `monitor.list` result. Empty `monitors` is authoritative only in `Listed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorListOutcomeWire {
    Listed {
        #[serde(default)]
        monitors: Vec<MonitorRegistrationWire>,
    },
    Rejected {
        rejection: MonitorControlRejectionWire,
    },
    #[serde(other)]
    Unknown,
}

/// Typed `monitor.list` receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorListReceiptWire {
    pub session_id: SessionId,
    pub policy: MonitorControlPolicyWire,
    pub sources: Vec<MonitorSourceAvailabilityWire>,
    pub outcome: MonitorListOutcomeWire,
}

/// `monitor.register` result. Rejections are data, never an untyped string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorRegisterOutcomeWire {
    Registered {
        monitor: MonitorRegistrationWire,
    },
    Rejected {
        rejection: MonitorControlRejectionWire,
    },
    #[serde(other)]
    Unknown,
}

/// Typed, command-correlated `monitor.register` receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorRegisterReceiptWire {
    pub command_id: CommandId,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub policy: MonitorControlPolicyWire,
    pub sources: Vec<MonitorSourceAvailabilityWire>,
    pub outcome: MonitorRegisterOutcomeWire,
}

/// `monitor.remove` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorRemoveOutcomeWire {
    Removed {
        monitor_id: String,
    },
    Rejected {
        rejection: MonitorControlRejectionWire,
    },
    #[serde(other)]
    Unknown,
}

/// Typed, command-correlated `monitor.remove` receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorRemoveReceiptWire {
    pub command_id: CommandId,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub policy: MonitorControlPolicyWire,
    pub sources: Vec<MonitorSourceAvailabilityWire>,
    pub outcome: MonitorRemoveOutcomeWire,
}

/// `monitor.watch` registration result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorWatchOutcomeWire {
    Watching {
        watch_id: String,
        requested_after_cursor: u64,
        replay_through_cursor: u64,
    },
    Rejected {
        rejection: MonitorControlRejectionWire,
    },
    #[serde(other)]
    Unknown,
}

/// Typed `monitor.watch` receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorWatchReceiptWire {
    pub session_id: SessionId,
    pub policy: MonitorControlPolicyWire,
    pub sources: Vec<MonitorSourceAvailabilityWire>,
    pub outcome: MonitorWatchOutcomeWire,
}

/// Stable rejection reasons for `loom.install.retry`. Job lookup, lifecycle
/// state, and current-contract identity remain distinct facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TypedAgentInstallRetryRejectionWire {
    JobNotFound,
    StateNotRetryable {
        state: haider_protocol::typed_agent::TypedAgentInstallState,
    },
    ContractNotCurrent,
    #[serde(other)]
    Unknown,
}

/// Typed result of explicitly retrying one durable install job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TypedAgentInstallRetryOutcomeWire {
    Requeued {
        job: haider_protocol::typed_agent::TypedAgentInstallJob,
    },
    Rejected {
        rejection: TypedAgentInstallRetryRejectionWire,
    },
    #[serde(other)]
    Unknown,
}

/// Typed `loom.install.retry` receipt. `job_id` is the opaque install-job
/// coordinate supplied by the client, never an agent/workflow id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedAgentInstallRetryReceiptWire {
    pub job_id: String,
    pub outcome: TypedAgentInstallRetryOutcomeWire,
}

/// Stable rejection reasons for the exact-job progress watch door.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TypedAgentInstallWatchRejectionWire {
    JobNotFound,
    CursorAhead {
        requested: u64,
        head: u64,
    },
    #[serde(other)]
    Unknown,
}

/// One replay page from an install job's durable progress history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TypedAgentInstallWatchOutcomeWire {
    Watching {
        requested_after_cursor: u64,
        replay_through_cursor: u64,
        next_cursor: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        events: Vec<haider_protocol::typed_agent::TypedAgentInstallEvent>,
    },
    Rejected {
        rejection: TypedAgentInstallWatchRejectionWire,
    },
    #[serde(other)]
    Unknown,
}

/// Typed `loom.install.watch` receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedAgentInstallWatchReceiptWire {
    pub job_id: String,
    pub outcome: TypedAgentInstallWatchOutcomeWire,
}

/// Typed result of cancelling one exact durable install job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TypedAgentInstallCancelOutcomeWire {
    Cancelled,
    AlreadyTerminal {
        state: haider_protocol::typed_agent::TypedAgentInstallTerminalStateV1,
    },
    /// The exact requested durable job is absent. Future unrecognized status
    /// spellings also fail closed here for older readers.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedAgentInstallCancelReceiptWire {
    pub install_job_id: String,
    pub outcome: TypedAgentInstallCancelOutcomeWire,
}

/// Typed archive/unarchive result. `Already` carries the exact current fact;
/// `NotFound` is an explicit absence rather than a fabricated empty entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LoomArchiveOutcomeWire {
    Changed {
        entry: haider_protocol::loom::LoomRegistryEntryRef,
    },
    Already {
        entry: haider_protocol::loom::LoomRegistryEntryRef,
    },
    NotFound,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoomArchiveReceiptWire {
    pub kind: haider_protocol::loom::LoomRegistryEntryKind,
    pub id: String,
    pub outcome: LoomArchiveOutcomeWire,
}

/// Report terminal/match status from the durable monitor subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorReportStatusWire {
    Matched,
    RateLimited,
    TimedOut,
    #[serde(other)]
    Unknown,
}

/// Source event payload retained by one bounded report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MonitorEventPayloadWire {
    Sms {
        address: String,
        body: String,
        received_at_ms: i64,
    },
    Process {
        line: String,
    },
    File {
        payload: String,
    },
    Poll {
        payload: String,
    },
    Timer {
        fired_at_ms: u64,
    },
    #[serde(other)]
    Unknown,
}

/// One source observation in a delivery report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorEventWire {
    pub sequence: u64,
    pub observed_at_ms: u64,
    pub payload: MonitorEventPayloadWire,
}

/// Stable identities for exact-redelivery suppression and revision grouping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorDeliveryDedupeWire {
    /// Unique to one durable journal revision (`session_id` + `cursor`).
    pub delivery_key: String,
    /// Stable across coalesced revisions of the same report.
    pub report_key: String,
}

/// One bounded, durable monitor delivery revision. `cursor` is the owning
/// session journal sequence of `MonitorReportPending`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorDeliveryReportWire {
    pub report_id: String,
    pub monitor_id: String,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    pub source: MonitorSourceKindWire,
    pub status: MonitorReportStatusWire,
    #[serde(default)]
    pub events: Vec<MonitorEventWire>,
    pub coalesced_count: u64,
    pub omitted_count: u64,
    pub action: MonitorActionWire,
    pub cursor: u64,
    pub dedupe: MonitorDeliveryDedupeWire,
}

/// v0.1 request method bodies.
///
/// The internally tagged method object keeps each operation visibly named and
/// avoids JSON-RPC's method/params semantics. Unknown future methods decode to
/// [`RequestBody::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method")]
#[non_exhaustive]
pub enum RequestBody {
    /// Requests graceful shutdown of this authenticated profile daemon.
    /// The daemon accepts it only from a connection granted Control.
    #[serde(rename = "daemon.shutdown")]
    DaemonShutdown {},
    /// Lists the shared command catalog for the requesting surface's exact
    /// current context.
    #[serde(rename = "command.list")]
    CommandList {
        query: String,
        in_session: bool,
        #[serde(default, skip_serializing_if = "CommandDynamicSlotsWire::is_empty")]
        slots: CommandDynamicSlotsWire,
    },
    /// Invokes one shared-catalog command. Durable operations use the
    /// caller's command id as their idempotency key.
    #[serde(rename = "command.invoke")]
    CommandInvoke {
        command_id: CommandId,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },
    /// Receipt-free byte ingress into the daemon-owned content-addressed
    /// store. Repeating the same decoded bytes is naturally idempotent.
    #[serde(rename = "artifact.put")]
    ArtifactPut {
        /// RFC 4648 standard-alphabet base64, decoded before the hard byte
        /// cap is applied and before the CAS address is computed.
        data_base64: String,
    },
    /// Additive source-compatible form of `session.create`. The legacy Rust
    /// variant below remains serializable for existing callers, while wire
    /// decoding normalizes both old and new JSON into this variant.
    #[serde(rename = "session.create")]
    SessionCreateWithPermissionOverrides {
        command_id: CommandId,
        cwd: String,
        provider: String,
        model: String,
        max_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_overrides: Option<SessionPermissionOverridesV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_policy: Option<haider_protocol::cache::CachePolicySettingsV1>,
        #[serde(
            default,
            skip_serializing_if = "haider_protocol::session::SessionInteractionModeV1::is_interactive"
        )]
        interaction_mode: haider_protocol::session::SessionInteractionModeV1,
    },
    /// Atomically creates typed session configuration, a `Created` event, and
    /// the durable command receipt that makes response-loss retries safe.
    ///
    /// This encode-only compatibility variant keeps existing Rust callers
    /// source-compatible. Decoders produce
    /// [`Self::SessionCreateWithPermissionOverrides`] with `None`.
    #[serde(rename = "session.create", skip_deserializing)]
    SessionCreate {
        command_id: CommandId,
        cwd: String,
        provider: String,
        model: String,
        max_tokens: u64,
    },
    /// Cursor-paginated, non-subscribing session listing.
    ///
    /// v0.1 ordering is the immutable `session_id` in ascending byte order.
    /// `cursor` is an opaque server token positioned after the last emitted
    /// ordering key; clients must return it verbatim and never parse it as an
    /// array offset.
    #[serde(rename = "session.list")]
    SessionList {
        /// Omitted for the first page; otherwise the prior response's token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        /// Maximum number of summaries to return.
        limit: u32,
    },
    /// Starts a connection-scoped watch of changed/new session summaries.
    #[serde(rename = "session.list_watch")]
    SessionListWatch {},
    /// Publishes one or both daemon-owned volatile session surfaces. An
    /// omitted field leaves that surface unchanged; empty text is a value.
    #[serde(rename = "session.surface_publish")]
    SessionSurfacePublish {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<SurfaceInputPublishWire>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<SurfaceStatusPublishWire>,
    },
    /// Watches the complete latest volatile surface snapshot for one session.
    #[serde(rename = "session.surface_watch")]
    SessionSurfaceWatch { session_id: SessionId },
    /// Routes an input operation to the current live input owner.
    #[serde(rename = "session.input_inject")]
    SessionInputInject {
        session_id: SessionId,
        op: SurfaceInjectOp,
    },
    /// Resolves the daemon-owned native JSONL sidecar path for one session.
    /// Clients must not derive this path from the raw session id.
    #[serde(rename = "session.pipe_path")]
    SessionPipePath { session_id: SessionId },
    /// Non-subscribing read of committed envelopes in an inclusive range.
    #[serde(rename = "session.read")]
    SessionRead {
        session_id: SessionId,
        range: SeqRange,
    },
    /// Returns a bounded, secret-free state digest derived from the committed
    /// journal. `last_event_limit` affects only the trailing kind names.
    #[serde(rename = "session.observe")]
    SessionObserve {
        session_id: SessionId,
        #[serde(default)]
        last_event_limit: u32,
        /// Additive fast path (v0.0.935 #7): when set, the daemon may skip
        /// the full-replay projection — `metadata`, `title`, `head_seq`, and
        /// `worker_generation` stay authoritative while every other projected
        /// field is default/empty. Old daemons ignore the field and serve the
        /// full digest, so callers reading only the authoritative fields see
        /// identical values either way.
        #[serde(default, skip_serializing_if = "is_false")]
        metadata_only: bool,
    },
    /// Batched form of `session.observe`; order is preserved exactly.
    #[serde(rename = "session.observe_batch")]
    SessionObserveBatch {
        session_ids: Vec<SessionId>,
        #[serde(default)]
        last_event_limit: u32,
        #[serde(default, skip_serializing_if = "is_false")]
        metadata_only: bool,
    },
    /// Returns the bounded full descendant tree and daemon-side rollup from
    /// durable delegation and child-journal truth. Read-only and receipt-free.
    #[serde(rename = "session.fleet")]
    SessionFleet { session_id: SessionId },
    #[serde(rename = "graph.pin")]
    GraphPin {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_digest: Option<String>,
    },
    #[serde(rename = "graph.run_set.open")]
    GraphRunSetOpen {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        plan_item_id: ItemId,
        plan_event_seq: u64,
    },
    #[serde(rename = "graph.switch")]
    GraphSwitch {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        old_graph_id: GraphId,
        template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_digest: Option<String>,
    },
    #[serde(rename = "graph.abandon")]
    GraphAbandon {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        why: String,
    },
    #[serde(rename = "graph.status")]
    GraphStatus { session_id: SessionId },
    /// B1 — read the Loom registry (agent types + compiled workflows).
    #[serde(rename = "loom.list")]
    LoomList {
        #[serde(default, skip_serializing_if = "is_false")]
        include_archived: bool,
    },
    /// B1 — register/revise one agent type. The registry owns revs: identical
    /// content no-ops, changed content advances by one.
    #[serde(rename = "loom.register_agent_type")]
    LoomRegisterAgentType {
        record: haider_protocol::loom::LoomAgentType,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_rev: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_digest: Option<String>,
    },
    /// Durable required-CLI installation status. Both filters omitted lists
    /// the bounded newest retained jobs; either filter narrows that view.
    #[serde(rename = "loom.install.status")]
    LoomInstallStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_type_id: Option<String>,
    },
    /// B1 — register/revise one workflow FROM PIPE SOURCE; the daemon
    /// compiles it against the current agent-type registry and rejects a bad
    /// pipe with the full error list.
    #[serde(rename = "loom.register_workflow")]
    LoomRegisterWorkflow {
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_rev: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_digest: Option<String>,
    },
    #[serde(rename = "graph.inspect")]
    GraphInspect {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        limit: u32,
    },
    /// Persists a client-detected compatibility fault in the session journal.
    #[serde(rename = "session.diagnostic")]
    SessionDiagnostic {
        command_id: CommandId,
        session_id: SessionId,
        code: String,
        message: String,
    },
    /// Discovers the effective hooks for one canonicalizable workspace.
    #[serde(rename = "hooks.list")]
    HooksList { cwd: String },
    /// Receipt-backed digest pin.
    #[serde(rename = "hooks.trust")]
    HooksTrust {
        command_id: CommandId,
        digest: String,
    },
    /// Receipt-backed digest revocation.
    #[serde(rename = "hooks.revoke")]
    HooksRevoke {
        command_id: CommandId,
        digest: String,
    },
    /// The only operation that begins event delivery. `after_seq` is the
    /// greatest sequence the client has fully applied (zero for complete
    /// history); the daemon replays strictly after it.
    #[serde(rename = "session.attach")]
    SessionAttach {
        session_id: SessionId,
        after_seq: u64,
        mode: AttachMode,
        /// Omits item deltas from the initial durable replay only. Buffered
        /// and live delivery after `AttachCaughtUp` remain unfiltered.
        #[serde(default, skip_serializing_if = "is_false")]
        sealed_replay: bool,
    },
    /// Ends event delivery for one attachment; never affects session
    /// authority or worker ownership.
    #[serde(rename = "session.detach")]
    SessionDetach { attachment_id: AttachmentId },
    /// Atomically creates one durable named ref at an exact committed node.
    #[serde(rename = "branch.create")]
    BranchCreate {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_branch_id: Option<BranchId>,
        fork_node_id: NodeId,
        fork_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Creates a complete new session from one exact source-history node.
    /// This is session-level cloning, not `branch.create`.
    #[serde(rename = "session.fork")]
    SessionFork {
        command_id: CommandId,
        /// Source session; the child id is daemon-minted and receipt-stable.
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_branch_id: Option<BranchId>,
        fork_node_id: NodeId,
        fork_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Review-gated session fork with model-proposed prompt omissions.
    ///
    /// The first call omits `accepted_proposal_digest`; it is read-only and
    /// returns the canonical digest plus the exact proposal for operator
    /// review. Echoing the complete review-manifest digest on the same
    /// connection is the human acceptance proof and is the only form allowed
    /// to claim a receipt and create the child.
    #[serde(rename = "session.metafork")]
    SessionMetafork {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_branch_id: Option<BranchId>,
        fork_node_id: NodeId,
        fork_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        description: String,
        model_proposal: SessionMetaforkProposal,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Digest of the complete source/name/description/proposal review.
        accepted_proposal_digest: Option<String>,
    },
    /// Message one direct child of the named parent session. The daemon
    /// chooses current-round STEER versus an immediate fresh child turn.
    #[serde(rename = "agent.message")]
    AgentMessage {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        agent: AgentId,
        text: String,
    },
    /// Branch-capable decode form of `turn.submit`.
    #[serde(rename = "turn.submit")]
    TurnSubmitWithBranch {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_id: Option<BranchId>,
        text: String,
        #[serde(default)]
        attachments: Vec<AttachmentBlock>,
        mode: DeliveryMode,
    },
    /// Encode-only source-compatible main-branch turn submission. Decoders
    /// normalize both old and new JSON into [`Self::TurnSubmitWithBranch`].
    #[serde(rename = "turn.submit", skip_deserializing)]
    TurnSubmit {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        text: String,
        #[serde(default)]
        attachments: Vec<AttachmentBlock>,
        mode: DeliveryMode,
    },
    /// Feature-gated resident CLI door. It enters the identical receipted
    /// turn admission path without requiring a new process or daemon socket.
    #[serde(rename = "turn.submit_from_cli")]
    TurnSubmitFromCli {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_id: Option<BranchId>,
        text: String,
        #[serde(default)]
        attachments: Vec<AttachmentBlock>,
        mode: DeliveryMode,
    },
    /// Additive headless submission carrying a run-scoped hook trust grant.
    /// The distinct method preserves source compatibility for older Rust
    /// callers while keeping omission on ordinary submissions byte-stable.
    #[serde(rename = "turn.submit_with_hook_trust")]
    TurnSubmitWithHookTrust {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_id: Option<BranchId>,
        text: String,
        #[serde(default)]
        attachments: Vec<AttachmentBlock>,
        mode: DeliveryMode,
    },
    /// Complete render-ready snapshot of messages held behind this session's
    /// active turn. Failure or feature absence is never an empty snapshot.
    #[serde(rename = "queue.list")]
    QueueList { session_id: SessionId },
    /// Removes exactly the stable row named by `id` if `revision` still names
    /// the current queue snapshot.
    #[serde(rename = "queue.remove")]
    QueueRemove {
        session_id: SessionId,
        id: EventId,
        revision: u64,
    },
    /// Converts one held row to active-turn steer delivery, fenced by the
    /// queue revision exactly like [`Self::QueueRemove`].
    #[serde(rename = "queue.promote_steer")]
    QueuePromoteSteer {
        session_id: SessionId,
        id: EventId,
        revision: u64,
    },
    /// Durably records cancellation intent before waking the worker.
    #[serde(rename = "turn.cancel")]
    TurnCancel {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        run_id: RunId,
    },
    /// Starts a fresh run from the latest failed main-timeline user turn, or
    /// wakes the exact current automatic provider backoff. No new
    /// `UserMessage` is committed in either case.
    #[serde(rename = "run.retry")]
    RunRetry {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
    },
    /// Branch-capable decode form of `session.compact`.
    #[serde(rename = "session.compact")]
    SessionCompactOnBranch {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_id: Option<BranchId>,
    },
    /// Encode-only source-compatible main-branch manual compaction.
    #[serde(rename = "session.compact", skip_deserializing)]
    SessionCompact {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
    },
    /// Receipted live-session model selection. Sessions are provider-agnostic:
    /// the user selects a MODEL, and the provider rides along as an attribute
    /// of the selected row. An absent `provider` keeps today's bytes and
    /// behavior — the model is selected within the session's current
    /// provider. A present `provider` selects a row served by that provider;
    /// the daemon validates creatability and, when a discovered inventory
    /// exists, membership. The next logical turn resolves through the
    /// committed pair (R6 re-resolution).
    #[serde(rename = "session.select_model")]
    SessionSelectModel {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        confirm_new_epoch: bool,
    },
    /// Receipted live-session rename (G2). `title` is normalized by the
    /// daemon (trimmed, control characters stripped, ≤ 80 chars; empty
    /// collapses to `None`); an absent/`None` title CLEARS the stored one.
    /// Same-command retries replay the committed receipt.
    #[serde(rename = "session.rename")]
    SessionRename {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Receipted durable acknowledgement that one surface has viewed this
    /// session. The daemon advances the timestamp monotonically and replays
    /// the original receipt for a repeated command id.
    #[serde(rename = "session.seen")]
    SessionSeen {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
    },
    /// Receipted live-session effort selection (G3), mirroring
    /// `session.select_model` exactly: receipt replay precedes validation,
    /// the store fences the worker generation, and the next logical turn
    /// resolves through the committed metadata. `effort: null` (absent)
    /// reverts to the provider default; a present value must be in the
    /// CURRENT pair's declared ladder.
    #[serde(rename = "session.select_effort")]
    SessionSelectEffort {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        confirm_new_epoch: bool,
    },
    /// Receipted live-session agent-type binding (W-flow inline identity).
    /// `agent_type: null` (absent) reverts to a plain session; a present id
    /// must exist in the Loom registry. The bound type's job rides the
    /// volatile prompt tail — the cache epoch is untouched either way.
    #[serde(rename = "session.select_agent_type")]
    SessionSelectAgentType {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_type: Option<String>,
    },
    /// Receipted live-session fast-mode toggle (G3), same law set as
    /// `session.select_effort`. Enabling requires the CURRENT pair to be in
    /// the static fast gate; disabling is always accepted.
    #[serde(rename = "session.select_fast")]
    SessionSelectFast {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        enabled: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        confirm_new_epoch: bool,
    },
    /// Scope-capable decode form of `shell.exec`. `branch_id`/`agent_id` bind
    /// the durable command record to the composer whose next turn consumes it.
    #[serde(rename = "shell.exec")]
    ShellExecScoped {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_id: Option<BranchId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<AgentId>,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Encode-only source-compatible unscoped direct shell request. Decoders
    /// normalize old JSON into [`Self::ShellExecScoped`].
    #[serde(rename = "shell.exec")]
    #[serde(skip_deserializing)]
    ShellExec {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Reads canonical registered manifests/defaults plus grants reconstructed
    /// from the target session's durable journal.
    #[serde(rename = "tools.inventory")]
    ToolsInventory { session_id: SessionId },
    /// Stages a raw secret in connection-scoped daemon memory and returns an
    /// opaque single-use reference (R7). Intentionally NON-durable: no
    /// command receipt may ever contain a secret. `stage_id` is an ephemeral
    /// client nonce for same-connection retry dedupe only: the same id with
    /// the same bytes returns the same reference; the same id with
    /// different bytes is invalid. Served only on authenticated same-UID
    /// local UDS connections with connection-level Control.
    #[serde(rename = "vault.stage")]
    VaultStage {
        stage_id: String,
        purpose: StagePurpose,
        secret: SecretWire,
    },
    /// Durable API-key login (R10): claims a staged secret, validates it,
    /// commits Keychain + descriptor recoverably, and answers with the
    /// descriptor. Command identity covers provider/resolved-model/alias and
    /// deliberately EXCLUDES the ephemeral `vault_reference`, so a
    /// lost-response retry may supply a freshly staged reference under the
    /// same command id and still recover the original committed result.
    /// `validation_model: None` means the release-owned full model ID in
    /// the resolved profile. `replace_existing` is an explicit recovery
    /// coordinate for in-place key rotation; omission keeps legacy add/login
    /// semantics.
    #[serde(rename = "account.login_api")]
    AccountLoginApi {
        command_id: CommandId,
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
        vault_reference: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validation_model: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        replace_existing: bool,
    },
    /// Starts a daemon-owned loopback authorization flow. The response is
    /// delivered asynchronously after the coordinator binds `127.0.0.1:0`;
    /// the connection task performs only authorization and bounded handoff.
    #[serde(rename = "account.oauth_start")]
    AccountOAuthStart {
        provider: String,
        desired_alias: String,
        attempt_id: String,
    },
    /// Reads only the public phase of a connection-bound flow.
    #[serde(rename = "account.oauth_status")]
    AccountOAuthStatus {
        flow_id: OAuthFlowId,
        attempt_id: String,
    },
    /// Idempotently cancels a connection-bound flow.
    #[serde(rename = "account.oauth_cancel")]
    AccountOAuthCancel {
        flow_id: OAuthFlowId,
        attempt_id: String,
    },
    /// Lists the daemon-owned sanctioned import sources and whether each
    /// daemon-local credential store is present and readable at call time.
    #[serde(rename = "account.oauth_import_sources")]
    AccountOAuthImportSources,
    /// Imports a sanctioned OAuth bundle from a daemon-local CLI credential
    /// store. Only the source name crosses the wire; token material is read
    /// and retained by the daemon.
    #[serde(rename = "account.oauth_import")]
    AccountOAuthImport {
        command_id: CommandId,
        source: String,
    },
    /// Reads only bounded, non-secret metadata from known first-party stores.
    #[serde(rename = "account.device_candidates")]
    AccountDeviceCandidates,
    /// Imports one candidate by opaque identifier. The daemon re-discovers
    /// and reads the local source; credential bytes never cross this frame.
    #[serde(rename = "account.import_device")]
    AccountImportDevice {
        command_id: CommandId,
        candidate: String,
    },
    /// Durable OAuth account creation. `oauth_reference` is transient,
    /// daemon-instance/connection-bound, single-use, and excluded from the
    /// semantic command digest.
    #[serde(rename = "account.add")]
    AccountAdd {
        command_id: CommandId,
        provider: String,
        alias: String,
        auth_method: AccountAddMethod,
        flow_id: OAuthFlowId,
        attempt_id: String,
        oauth_reference: OAuthReadyRefWire,
    },
    /// Durably selects the globally named account. The provider is
    /// intentionally absent: the daemon derives it from descriptor truth.
    #[serde(rename = "account.set_active")]
    AccountSetActive {
        command_id: CommandId,
        alias: String,
        #[serde(default, skip_serializing_if = "is_false")]
        confirm_new_epoch: bool,
    },
    /// Durably removes one globally named account.
    #[serde(rename = "account.remove")]
    AccountRemove {
        command_id: CommandId,
        alias: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<u64>,
    },
    /// Changes only a registered provider's default model.
    #[serde(rename = "account.set_default_model")]
    AccountSetDefaultModel {
        command_id: CommandId,
        provider: String,
        model: String,
        expected_revision: u64,
    },
    /// Set or clear one account's operator-chosen display label (Control).
    /// `label: null` clears it. Cosmetic only — the alias remains the
    /// identity every other door addresses, so a rename can never break a
    /// reference.
    #[serde(rename = "account.set_label")]
    AccountSetLabel {
        alias: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Watch for account-registry changes (View). The daemon answers
    /// `accepted`, then pushes an `AccountsChanged` event carrying the new
    /// revision whenever the management snapshot publishes — a change SIGNAL,
    /// not a delta stream: re-read `account.list` on notice.
    #[serde(rename = "account.list_watch")]
    AccountListWatch {},
    /// Lists credential descriptors (View); never secrets.
    #[serde(rename = "account.list")]
    AccountList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Lists provider management summaries from the daemon's published
    /// snapshot. This read never probes an endpoint inline.
    #[serde(rename = "provider.list")]
    ProviderList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Refreshes one OAuth provider's model inventory from the provider's own
    /// authenticated catalog.
    #[serde(rename = "provider.models_refresh")]
    ProviderModelsRefresh { provider: String },
    /// Creates a custom provider or safely updates mutable fields on an
    /// existing profile. API family and auth requirement are create-only;
    /// the `provider` key remains the stable identity. A custom provider's
    /// origin may be changed on update (under `expected_revision`); fixed
    /// release-owned origins remain immutable except for explicitly
    /// shape-validated enterprise configuration surfaces.
    #[serde(rename = "provider.configure")]
    ProviderConfigure {
        command_id: CommandId,
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_family: Option<ProviderApiFamilyWire>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_requirement: Option<ProviderAuthRequirementWire>,
        enabled: bool,
        #[serde(default)]
        models: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_model: Option<String>,
        /// OpenAI-family response-header wait. Omission preserves the stored
        /// value on update and selects the 60-second default on create.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_open_timeout_ms: Option<u64>,
        /// Ephemeral staged API key used only to authenticate model
        /// discovery before this mutation is accepted. The daemon borrows
        /// the connection-scoped stage without consuming it, excludes the
        /// reference and bytes from durable command identity/recovery, and
        /// leaves the same reference available to `account.login_api`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        probe_vault_reference: Option<String>,
        expected_revision: u64,
    },
    /// Durably removes one custom provider. Release-owned providers and
    /// providers referenced by any credential descriptor are refused.
    #[serde(rename = "provider.remove")]
    ProviderRemove {
        command_id: CommandId,
        provider: String,
        expected_revision: u64,
    },
    /// Reads the profile's vaulted transcription secret (the Deepgram API
    /// key) for the TUI-resident engine. Served ONLY on authenticated
    /// same-UID local UDS connections with Control — the raw secret answer
    /// rides the same protected surface as `vault.stage`, and both codecs
    /// zeroize the encoded frame buffers around it.
    #[serde(rename = "transcription.secret_get")]
    TranscriptionSecretGet,
    /// Stores or clears the profile's transcription secret in the daemon
    /// vault (FileVault, profile-scoped alias). `clear: true` requires an
    /// EMPTY `secret` and deletes the entry; otherwise the secret must be
    /// non-empty, ≤512 chars, with no control bytes (ADE key hygiene).
    /// UDS-only, like every raw-secret surface. Deliberately NON-durable
    /// command-wise: no receipt may ever contain a secret; the vault file
    /// itself is the durable truth.
    #[serde(rename = "transcription.secret_set")]
    TranscriptionSecretSet {
        secret: SecretWire,
        #[serde(default)]
        clear: bool,
    },
    /// Reads the cross-provider usage snapshot: one entry per known account
    /// with normalized OAuth meter windows or honest local-only/unavailable
    /// states, plus journal-derived local counters. Read-only, receipt-free,
    /// and parameterless in v1.
    #[serde(rename = "usage.report")]
    UsageReport,
    /// Reads one device-local UTC day. `YYYY-MM-DD` validation is performed
    /// by the store; a missing day is a successful `day: null` response.
    #[serde(rename = "usage.history_day")]
    UsageHistoryDay { date: String },
    /// Reads exactly `days` dated heatmap cells ending at `through_date`.
    #[serde(rename = "usage.history_range")]
    UsageHistoryRange { through_date: String, days: u16 },
    /// Opens the server-known System Settings pane for an unresolved durable
    /// computer permission request. No caller-provided URL is accepted.
    #[serde(rename = "computer.permission_open_settings")]
    ComputerPermissionOpenSettings {
        session_id: SessionId,
        request_id: String,
        permission: haider_protocol::permission::SystemPermission,
    },
    /// Resolves the current instance by id, or an exact retained revision
    /// when `template_digest` comes from a pinned graph fact.
    #[serde(rename = "workflow.instance")]
    WorkflowInstance {
        workflow_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        template_digest: Option<String>,
    },
    /// Attaches a reconnectable live view of the durable descendant tree.
    /// Each cursor is scoped by both child session and agent identity.
    #[serde(rename = "session.descendants.attach")]
    SessionDescendantsAttach {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cursors: Vec<DescendantReplayCursorWire>,
        max_children: u32,
    },
    /// Read the durable monitor registry for one session. View is sufficient.
    #[serde(rename = "monitor.list")]
    MonitorList { session_id: SessionId },
    /// Register one monitor through the same source/filter/action vocabulary
    /// as the model tool. Control is required and `command_id` is the durable
    /// retry identity.
    #[serde(rename = "monitor.register")]
    MonitorRegister {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        source: MonitorSourceWire,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<MonitorFilterWire>,
        action: MonitorActionWire,
        #[serde(default)]
        occurrence: MonitorOccurrenceWire,
        #[serde(default)]
        lifetime: MonitorLifetimeWire,
    },
    /// Remove one monitor from its owning session. Control is required and
    /// retries use the durable command receipt.
    #[serde(rename = "monitor.remove")]
    MonitorRemove {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        monitor_id: String,
    },
    /// Start a session-scoped delivery replay strictly after the greatest
    /// journal cursor the client has fully applied.
    #[serde(rename = "monitor.watch")]
    MonitorWatch {
        session_id: SessionId,
        after_cursor: u64,
    },
    /// Explicitly reset one failed, current-contract install job to queued.
    #[serde(rename = "loom.install.retry")]
    LoomInstallRetry { job_id: String },
    /// Read durable progress snapshots strictly after the applied cursor.
    #[serde(rename = "loom.install.watch")]
    LoomInstallWatch { job_id: String, after_cursor: u64 },
    /// Accepts a headless run with a fully resolved, durable execution pin.
    #[serde(rename = "headless.run.start")]
    HeadlessRunStart {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        text: String,
        #[serde(default)]
        attachments: Vec<AttachmentBlock>,
        spec: HeadlessRunSpecV1,
        #[serde(default)]
        trust_hooks: bool,
    },
    /// Resolves durable lifecycle coordinates from a globally unique run id.
    #[serde(rename = "headless.run.status")]
    HeadlessRunStatus { run_id: RunId },
    /// Idempotently stops a detached run after daemon-owned run lookup.
    #[serde(rename = "headless.run.stop")]
    HeadlessRunStop {
        command_id: CommandId,
        run_id: RunId,
    },
    /// Indexed state of one typed workflow activation graph. Omission picks
    /// the session's most recently changed activation graph.
    #[serde(rename = "workflow.graph.state")]
    WorkflowGraphState {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_id: Option<GraphId>,
    },
    /// Replays durable activation facts strictly after the applied cursor.
    #[serde(rename = "workflow.graph.watch")]
    WorkflowGraphWatch {
        session_id: SessionId,
        after_cursor: u64,
        limit: u32,
    },
    /// Start a Loom authoring session from user prose.
    #[serde(rename = "loom.author.draft")]
    LoomAuthorDraft {
        /// Supplies the provider/model selection used for the AI draft. The
        /// authoring exchange does not append to this session's journal.
        session_id: SessionId,
        kind: haider_protocol::loom::LoomAuthorKind,
        prose: String,
    },
    /// Re-parse and re-validate the user's exact edited text.
    #[serde(rename = "loom.author.revise")]
    LoomAuthorRevise {
        authoring_id: String,
        expected_revision: u64,
        kind: haider_protocol::loom::LoomAuthorKind,
        text: String,
    },
    /// Confirm one validated revision and register its immutable hash.
    #[serde(rename = "loom.author.confirm")]
    LoomAuthorConfirm {
        authoring_id: String,
        expected_revision: u64,
        kind: haider_protocol::loom::LoomAuthorKind,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_rev: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_digest: Option<String>,
    },
    /// Cancel one queued/running durable required-CLI install job.
    #[serde(rename = "loom.install.cancel")]
    LoomInstallCancel { install_job_id: String },
    #[serde(rename = "loom.archive")]
    LoomArchive {
        kind: haider_protocol::loom::LoomRegistryEntryKind,
        id: String,
        expected_rev: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_digest: Option<String>,
    },
    #[serde(rename = "loom.unarchive")]
    LoomUnarchive {
        kind: haider_protocol::loom::LoomRegistryEntryKind,
        id: String,
        expected_rev: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_digest: Option<String>,
    },
    /// Validate exact editor text without registering it.
    #[serde(rename = "loom.validate")]
    LoomValidate {
        kind: haider_protocol::loom::LoomAuthorKind,
        text: String,
    },
    /// Attach a connection-scoped registry baseline + durable delta stream.
    #[serde(rename = "loom.watch")]
    LoomWatch { after_cursor: u64 },
    #[serde(rename = "checkpoint.list")]
    CheckpointList {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_id: Option<BranchId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<haider_protocol::checkpoint::CheckpointCursor>,
        limit: u16,
    },
    #[serde(rename = "checkpoint.undo")]
    CheckpointUndo {
        command_id: CommandId,
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_id: Option<BranchId>,
        worker_generation: u64,
        /// A checkpoint id or the literal `last`.
        target: String,
    },
    #[serde(rename = "checkpoint.redo")]
    CheckpointRedo {
        command_id: CommandId,
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_id: Option<BranchId>,
        worker_generation: u64,
        /// A checkpoint id or the literal `last`.
        target: String,
    },
    #[serde(rename = "checkpoint.rollback_turn")]
    CheckpointRollbackTurn {
        command_id: CommandId,
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_id: Option<BranchId>,
        worker_generation: u64,
        run_id: RunId,
    },
    /// Decode artifact for a method this crate does not know (tolerance
    /// discipline). W3b answers it with a protocol error, not a panic.
    #[serde(other)]
    Unknown,
}

/// v0.1 response method bodies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method")]
#[non_exhaustive]
pub enum ResponseBody {
    /// The graceful shutdown request was accepted. `ServerDraining` and
    /// disconnect remain the authoritative completion sequence.
    #[serde(rename = "daemon.shutdown")]
    DaemonShutdown {},
    #[serde(rename = "command.list")]
    CommandList {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<CommandCatalogItemWire>,
    },
    #[serde(rename = "command.invoke")]
    CommandInvoke { outcome: CommandInvokeOutcomeWire },
    /// Verified content address and decoded byte count for `artifact.put`.
    #[serde(rename = "artifact.put")]
    ArtifactPut { artifact: ArtifactRef, bytes: u64 },
    /// Durable acceptance coordinates of an atomic `session.create` (R2):
    /// a same-command retry receives this exact body from its receipt.
    #[serde(rename = "session.create")]
    SessionCreate {
        session_id: SessionId,
        created_seq: u64,
        worker_generation: u64,
        metadata: SessionMetadataV1,
    },
    /// One page in the fixed `session_id` ascending order.
    #[serde(rename = "session.list")]
    SessionList {
        #[serde(default)]
        sessions: Vec<SessionSummary>,
        /// Omitted on the last page; otherwise pass verbatim as the next
        /// [`RequestBody::SessionList`] cursor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    /// Acknowledges a connection-scoped session roster watch.
    #[serde(rename = "session.list_watch")]
    SessionListWatch { accepted: bool },
    /// Reports exactly which supplied revisions were accepted. `None` means
    /// that field was omitted or its publisher-local revision was stale.
    #[serde(rename = "session.surface_publish")]
    SessionSurfacePublished {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accepted_input_revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accepted_status_revision: Option<u64>,
    },
    /// Acknowledges a surface watch with its current complete snapshot.
    #[serde(rename = "session.surface_watch")]
    SessionSurfaceWatching {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<SurfaceInputWire>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<SurfaceStatusWire>,
    },
    /// Whether an input operation entered the current owner's outbox.
    #[serde(rename = "session.input_inject")]
    SessionInputInjectAck {
        session_id: SessionId,
        delivered: bool,
    },
    /// Absolute daemon-resolved native JSONL sidecar path.
    #[serde(rename = "session.pipe_path")]
    SessionPipePath { path: String },
    #[serde(rename = "session.read")]
    SessionRead { result: SessionReadResult },
    #[serde(rename = "session.observe")]
    SessionObserve { digest: SessionObserveDigest },
    #[serde(rename = "session.observe_batch")]
    SessionObserveBatch { digests: Vec<SessionObserveDigest> },
    #[serde(rename = "session.fleet")]
    SessionFleet { snapshot: SessionFleetSnapshot },
    /// One atomic snapshot cut. `rows` are complete display values and ordered
    /// by their one-based ordinal.
    #[serde(rename = "queue.list")]
    QueueList {
        session_id: SessionId,
        revision: u64,
        #[serde(default)]
        rows: Vec<QueueRow>,
    },
    #[serde(rename = "queue.remove")]
    QueueRemove {
        session_id: SessionId,
        id: EventId,
        revision: u64,
    },
    #[serde(rename = "queue.promote_steer")]
    QueuePromoteSteer {
        session_id: SessionId,
        id: EventId,
        revision: u64,
    },
    #[serde(rename = "graph.pin")]
    GraphPin {
        session_id: SessionId,
        graph_id: GraphId,
        template: String,
        digest: String,
        pinned_seq: u64,
        opened_seq: u64,
        worker_generation: u64,
    },
    #[serde(rename = "graph.run_set.open")]
    GraphRunSetOpen {
        session_id: SessionId,
        run_set_id: GraphRunSetId,
        root_graph_id: GraphId,
        plan_item_id: ItemId,
        plan_event_seq: u64,
        template: String,
        digest: String,
        run_set_opened_seq: u64,
        through_seq: u64,
        #[serde(default)]
        children: Vec<TodoGraphOpenedWire>,
        worker_generation: u64,
    },
    #[serde(rename = "graph.switch")]
    GraphSwitch {
        session_id: SessionId,
        old_graph_id: GraphId,
        new_graph_id: GraphId,
        template: String,
        digest: String,
        superseded_seq: u64,
        pinned_seq: u64,
        opened_seq: u64,
        worker_generation: u64,
    },
    #[serde(rename = "graph.abandon")]
    GraphAbandon {
        session_id: SessionId,
        graph_id: GraphId,
        abandoned_seq: u64,
        worker_generation: u64,
    },
    #[serde(rename = "graph.status")]
    GraphStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ConvergenceGraphStatus>,
    },
    /// B1 — the Loom registry contents.
    #[serde(rename = "loom.list")]
    LoomList {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        agent_types: Vec<haider_protocol::loom::LoomAgentType>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        workflows: Vec<haider_protocol::loom::LoomWorkflow>,
        /// Device PATH presence for every CLI any registered type declares,
        /// probed at list time. This is advisory inventory; the durable
        /// install job remains the authoritative typed-executor readiness
        /// gate and is observed through `loom.install.status`.
        ///
        /// Keyed by the declared name verbatim. Absent from the map means
        /// NOT PROBED (an older daemon, or a name that reached the client
        /// some other way) — which is not the same as absent from the
        /// device, and clients must not render it as missing.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        cli_present: std::collections::BTreeMap<String, bool>,
        /// Built-in and user workflow records from their daemon authorities.
        /// The field is meaningful only when `workflow_catalog_v1` was
        /// advertised; default + skip-empty preserves pre-feature v1 bytes.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        workflow_catalog: Vec<WorkflowCatalogEntryV1>,
        /// Present only when the request explicitly included archived entries.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        archived_entries: Vec<haider_protocol::loom::LoomRegistryEntryRef>,
    },
    /// B1 — a committed (or no-op) registration.
    #[serde(rename = "loom.registered")]
    LoomRegistered {
        registration: haider_protocol::loom::LoomRegistration,
        /// Present only under `typed_agent_install_control_v1` and only when
        /// this agent-type revision has a durable required-CLI install job.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        install_job_id: Option<String>,
    },
    #[serde(rename = "loom.install.status")]
    LoomInstallStatus {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        jobs: Vec<haider_protocol::typed_agent::TypedAgentInstallJob>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<haider_protocol::typed_agent::TypedAgentInstallItem>,
    },
    #[serde(rename = "graph.inspect")]
    GraphInspect {
        snapshot: GraphInspectSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    #[serde(rename = "session.diagnostic")]
    SessionDiagnostic { recorded_seq: u64 },
    #[serde(rename = "hooks.list")]
    HooksList {
        policy: String,
        /// Monotonic count of committed hook trust mutations. Defaults to
        /// zero when an older daemon omits it.
        #[serde(default)]
        revision: u64,
        #[serde(default)]
        hooks: Vec<HookSummaryWire>,
    },
    #[serde(rename = "hooks.trust")]
    HooksTrust { digest: String, trusted: bool },
    #[serde(rename = "hooks.revoke")]
    HooksRevoke { digest: String, trusted: bool },
    #[serde(rename = "session.attach")]
    SessionAttach {
        attachment_id: AttachmentId,
        attach_state: AttachState,
    },
    #[serde(rename = "session.detach")]
    SessionDetach { attachment_id: AttachmentId },
    /// Stable, secret-free coordinates of an atomic `branch.create` (R2).
    #[serde(rename = "branch.create")]
    BranchCreate {
        session_id: SessionId,
        branch_id: BranchId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_branch_id: Option<BranchId>,
        fork_node_id: NodeId,
        fork_seq: u64,
        created_seq: u64,
        worker_generation: u64,
        name: String,
    },
    /// Stable receipt coordinates of a complete session-level fork.
    #[serde(rename = "session.fork")]
    SessionFork {
        session_id: SessionId,
        source_session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_branch_id: Option<BranchId>,
        fork_node_id: NodeId,
        fork_seq: u64,
        created_seq: u64,
        worker_generation: u64,
        metadata: SessionMetadataV1,
    },
    /// A metafork review (`committed=false`) or its stable committed receipt.
    /// Optional child fields are absent during the write-free review phase.
    #[serde(rename = "session.metafork")]
    SessionMetafork {
        committed: bool,
        source_session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_branch_id: Option<BranchId>,
        fork_node_id: NodeId,
        fork_seq: u64,
        description: String,
        model_proposal: SessionMetaforkProposal,
        /// Exact operation whose digest is awaiting/records acceptance.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review_manifest: Option<SessionMetaforkReviewManifest>,
        /// Digest of the complete review manifest (legacy field name retained
        /// within this new feature's v1 wire shape).
        proposal_digest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_generation: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<SessionMetadataV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        omission_count: Option<u64>,
    },
    #[serde(rename = "agent.message")]
    AgentMessage { receipt: AgentMessageReceipt },
    /// Durable acceptance coordinates of `turn.submit` (R3): `run_id` and
    /// the `UserMessage` sequence committed by the acceptance transaction.
    /// Socket order relative to that transaction's events is NOT promised —
    /// the durable coordinates, not frame order, close the correlation.
    #[serde(rename = "turn.submit")]
    TurnSubmit {
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        worker_generation: u64,
        disposition: SubmitDisposition,
    },
    /// Branch-pinned acceptance coordinates. Main-branch responses retain
    /// the legacy `turn.submit` shape byte-for-byte.
    #[serde(rename = "turn.submit.on_branch")]
    TurnSubmitOnBranch {
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        worker_generation: u64,
        branch_id: BranchId,
        disposition: SubmitDisposition,
    },
    /// Outcome of durable cancellation intent (R5). `terminal_seq` is
    /// present exactly when `status` is `already_terminal`, naming the
    /// run's committed terminal sequence.
    #[serde(rename = "turn.cancel")]
    TurnCancel {
        session_id: SessionId,
        run_id: RunId,
        status: CancelStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_seq: Option<u64>,
    },
    /// Durable coordinates of manual retry acceptance. For terminal failure,
    /// `run_id` is fresh and `accepted_seq` names its committed `run_retried`
    /// fact. During provider backoff, `run_id == failed_run_id` and
    /// `accepted_seq` names the existing `Retrying` fact being woken.
    #[serde(rename = "run.retry")]
    RunRetry {
        session_id: SessionId,
        run_id: RunId,
        failed_run_id: RunId,
        user_seq: u64,
        accepted_seq: u64,
        worker_generation: u64,
    },
    #[serde(rename = "session.compact")]
    SessionCompact {
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        worker_generation: u64,
    },
    #[serde(rename = "session.compact.on_branch")]
    SessionCompactOnBranch {
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        worker_generation: u64,
        branch_id: BranchId,
    },
    /// Durable coordinates of a committed model selection (R2): the RESOLVED
    /// pair — never an echo of the request — plus the committed journal
    /// sequence of the `model_selected` fact. A same-command retry receives
    /// this exact body from its receipt.
    #[serde(rename = "session.select_model")]
    SessionSelectModel {
        session_id: SessionId,
        provider: String,
        model: String,
        selected_seq: u64,
        worker_generation: u64,
    },
    /// Durable coordinates of a committed rename (G2): the NORMALIZED title
    /// — never an echo of the request — plus the committed journal sequence
    /// of the `session_renamed` fact. A same-command retry receives this
    /// exact body from its receipt.
    #[serde(rename = "session.rename")]
    SessionRename {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        renamed_seq: u64,
        worker_generation: u64,
    },
    /// Durable coordinates of a committed attention acknowledgement. A
    /// repeated command id receives this exact receipt.
    #[serde(rename = "session.seen")]
    SessionSeen {
        session_id: SessionId,
        seen_at_ms: u64,
        seen_seq: u64,
        worker_generation: u64,
    },
    /// Durable coordinates of a committed effort selection (G3/R2): the
    /// RESOLVED value plus the committed journal sequence of the
    /// `effort_selected` fact. A same-command retry receives this exact body
    /// from its receipt.
    #[serde(rename = "session.select_effort")]
    SessionSelectEffort {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        selected_seq: u64,
        worker_generation: u64,
    },
    /// Durable coordinates of a committed agent-type binding (W-flow/R2).
    #[serde(rename = "session.select_agent_type")]
    SessionSelectAgentType {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_type: Option<String>,
        selected_seq: u64,
        worker_generation: u64,
    },
    /// Durable coordinates of a committed fast-mode toggle (G3/R2).
    #[serde(rename = "session.select_fast")]
    SessionSelectFast {
        session_id: SessionId,
        enabled: bool,
        selected_seq: u64,
        worker_generation: u64,
    },
    /// Durable acceptance coordinates for one direct shell command. Terminal
    /// status and byte output arrive through the ordinary item event stream.
    #[serde(rename = "shell.exec")]
    ShellExec {
        session_id: SessionId,
        /// Synthetic run owned by this direct command. Additive cancellation
        /// coordinate: clients may pass it straight to `turn.cancel` without
        /// racing event-stream discovery.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
        item_id: ItemId,
        accepted_seq: u64,
        worker_generation: u64,
    },
    #[serde(rename = "tools.inventory")]
    ToolsInventory {
        session_id: SessionId,
        inventory: ToolInventorySnapshot,
    },
    /// Opaque staged-secret reference (R7): random, connection- and
    /// daemon-instance-scoped, single-use, and expired at
    /// `expires_at_ms` (absolute Unix ms). Disconnect or drain wipes it.
    #[serde(rename = "vault.stage")]
    VaultStage {
        stage_id: String,
        vault_reference: String,
        expires_at_ms: u64,
    },
    /// Committed login result (R10): the descriptor now active for its
    /// provider. A same-command retry receives this exact body from the
    /// durable receipt. Never carries secret material.
    #[serde(rename = "account.login_api")]
    AccountLoginApi {
        descriptor: haider_protocol::credential::CredentialDescriptor,
    },
    /// Start result. Unavailable registrations return this same structured
    /// shape with no flow/URL and a precise reason.
    #[serde(rename = "account.oauth_start")]
    AccountOAuthStart {
        availability: OAuthAvailabilityWire,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_id: Option<OAuthFlowId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authorization_url: Option<OAuthAuthorizationWire>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_origin: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loopback_port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<u64>,
        /// v0.0.938, additive: the RFC 8628 user code for a DEVICE flow, so a
        /// surface can display it beside the verification URL instead of
        /// parsing it back out of that URL's query string. Absent for
        /// loopback/PKCE flows and from older daemons. The code is a
        /// short-lived public pairing string, not a credential — it is
        /// useless without the device_code the daemon keeps.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_code: Option<String>,
    },
    #[serde(rename = "account.oauth_status")]
    AccountOAuthStatus {
        flow_id: OAuthFlowId,
        status: OAuthFlowStatusWire,
    },
    #[serde(rename = "account.oauth_cancel")]
    AccountOAuthCancel {
        flow_id: OAuthFlowId,
        status: OAuthFlowStatusWire,
    },
    #[serde(rename = "account.oauth_import_sources")]
    AccountOAuthImportSources { sources: Vec<OAuthImportSourceWire> },
    #[serde(rename = "account.oauth_import")]
    AccountOAuthImport {
        descriptor: haider_protocol::credential::CredentialDescriptor,
        revision: u64,
    },
    #[serde(rename = "account.device_candidates")]
    AccountDeviceCandidates {
        /// True is an honest configured-off state, not an empty-device claim.
        discovery_disabled: bool,
        #[serde(default)]
        candidates: Vec<DeviceCredentialCandidateWire>,
    },
    #[serde(rename = "account.import_device")]
    AccountImportDevice {
        descriptor: haider_protocol::credential::CredentialDescriptor,
        revision: u64,
    },
    #[serde(rename = "account.add")]
    AccountAdd {
        descriptor: haider_protocol::credential::CredentialDescriptor,
    },
    #[serde(rename = "account.set_active")]
    AccountSetActive {
        descriptor: haider_protocol::credential::CredentialDescriptor,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prior_alias: Option<haider_protocol::ids::CredentialAlias>,
        revision: u64,
    },
    #[serde(rename = "account.remove")]
    AccountRemove {
        removed_alias: haider_protocol::ids::CredentialAlias,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement_active_alias: Option<haider_protocol::ids::CredentialAlias>,
        revision: u64,
    },
    #[serde(rename = "account.set_default_model")]
    AccountSetDefaultModel {
        provider: ProviderSummaryWire,
        revision: u64,
    },
    /// The credential after its label changed, plus the new revision.
    #[serde(rename = "account.set_label")]
    AccountSetLabel {
        descriptor: haider_protocol::credential::CredentialDescriptor,
        revision: u64,
    },
    #[serde(rename = "account.list_watch")]
    AccountListWatch { accepted: bool },
    /// Credential descriptors (never secrets).
    #[serde(rename = "account.list")]
    AccountList {
        #[serde(default)]
        descriptors: Vec<haider_protocol::credential::CredentialDescriptor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        provider_active: Vec<ProviderActiveWire>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        provider_defaults: Vec<ProviderDefaultWire>,
        /// Omitted by older daemons. `Available` plus an empty descriptor
        /// list means genuinely empty; `Unavailable` means the empty legacy
        /// fields are not account truth.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        availability: Option<SnapshotAvailabilityWire>,
    },
    /// Provider management summaries and their coherent snapshot revision.
    #[serde(rename = "provider.list")]
    ProviderList {
        #[serde(default)]
        providers: Vec<ProviderSummaryWire>,
        revision: u64,
        /// Omitted by older daemons. Do not interpret legacy `revision: 0`
        /// as subsystem absence when this field is omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        availability: Option<SnapshotAvailabilityWire>,
    },
    #[serde(rename = "provider.models_refresh")]
    ProviderModelsRefresh {
        provider: ProviderSummaryWire,
        revision: u64,
    },
    #[serde(rename = "provider.configure")]
    ProviderConfigure {
        provider: ProviderSummaryWire,
        revision: u64,
    },
    #[serde(rename = "provider.remove")]
    ProviderRemove { provider: String, revision: u64 },
    /// The vaulted transcription secret, or `None` when no secret is
    /// stored. Only ever sent on the same-UID local UDS surface; the
    /// value's `Debug` is redacted and both peers zeroize the encoded
    /// buffers ([`SecretWire`] laws).
    #[serde(rename = "transcription.secret_get")]
    TranscriptionSecretGet {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<SecretWire>,
    },
    /// Post-commit vault state: `present` is true after a store, false
    /// after a clear. Never echoes the secret.
    #[serde(rename = "transcription.secret_set")]
    TranscriptionSecretSet { present: bool },
    /// Cross-provider usage snapshot (U1). Derived data only — meter
    /// readings, aliases, display identities, local counters; never secrets.
    #[serde(rename = "usage.report")]
    UsageReport {
        report: haider_protocol::usage::UsageReportV1,
        /// Omitted by older daemons. Do not interpret a legacy
        /// `generated_at_ms: 0` report as subsystem absence when omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        availability: Option<SnapshotAvailabilityWire>,
    },
    #[serde(rename = "usage.history_day")]
    UsageHistoryDay {
        date: String,
        /// Profile installation identity, present even when `day` is absent.
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        day: Option<haider_protocol::usage::UsageHistoryDayV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        availability: Option<SnapshotAvailabilityWire>,
    },
    #[serde(rename = "usage.history_range")]
    UsageHistoryRange {
        through_date: String,
        /// One profile-scoped provenance identity for every returned cell.
        device_id: String,
        days: Vec<haider_protocol::usage::UsageHistoryRangeDayV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        availability: Option<SnapshotAvailabilityWire>,
    },
    #[serde(rename = "computer.permission_open_settings")]
    ComputerPermissionOpenSettings {
        permission: haider_protocol::permission::SystemPermission,
    },
    /// Successful durable menu resolution. The same-command retry receives
    /// the original sequence; a different command receives
    /// [`ERROR_CODE_ALREADY_RESOLVED`] instead.
    #[serde(rename = "menu.answer")]
    MenuAnswer { resolution_seq: u64 },
    /// A request-correlated operation failure.
    ///
    /// Stable v0.1 codes include [`ERROR_CODE_CURSOR_AHEAD`],
    /// [`ERROR_CODE_CAPABILITY_DENIED`], [`ERROR_CODE_ALREADY_RESOLVED`],
    /// [`ERROR_CODE_NOT_FOUND`], [`ERROR_CODE_DRAINING`],
    /// [`ERROR_CODE_OVERLOADED`], and [`ERROR_CODE_SURFACE_TEXT_TOO_LARGE`].
    /// Unknown future string codes remain carryable by older clients.
    #[serde(rename = "error")]
    Error {
        /// Stable machine-readable `snake_case` code.
        code: String,
        /// Human-readable detail; never load-bearing for client behavior.
        message: String,
        /// Whether retrying after the stated condition changes may succeed.
        retryable: bool,
        /// Typed recovery coordinates for codes that carry them (report
        /// §5.4/§5.6): [`ERROR_CODE_CURSOR_AHEAD`] and
        /// [`ERROR_CODE_ALREADY_RESOLVED`] MUST attach their variant so a
        /// client can act without parsing `message`. `None` for codes with
        /// nothing structured to say, and on frames from older daemons.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<ErrorData>,
    },
    #[serde(rename = "workflow.instance")]
    WorkflowInstance {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<WorkflowInstanceV1>,
    },
    /// Establishes the connection-scoped descendant attachment. The complete
    /// baseline is enqueued before any frame naming `attachment_id`.
    #[serde(rename = "session.descendants.attach")]
    SessionDescendantsAttach {
        attachment_id: AttachmentId,
        baseline: SessionDescendantBaselineWire,
    },
    #[serde(rename = "monitor.list")]
    MonitorList { receipt: MonitorListReceiptWire },
    #[serde(rename = "monitor.register")]
    MonitorRegister { receipt: MonitorRegisterReceiptWire },
    #[serde(rename = "monitor.remove")]
    MonitorRemove { receipt: MonitorRemoveReceiptWire },
    #[serde(rename = "monitor.watch")]
    MonitorWatch { receipt: MonitorWatchReceiptWire },
    #[serde(rename = "loom.install.retry")]
    LoomInstallRetry {
        receipt: TypedAgentInstallRetryReceiptWire,
    },
    #[serde(rename = "loom.install.watch")]
    LoomInstallWatch {
        receipt: TypedAgentInstallWatchReceiptWire,
    },
    #[serde(rename = "headless.run.start")]
    HeadlessRunStart {
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        worker_generation: u64,
        disposition: SubmitDisposition,
    },
    #[serde(rename = "headless.run.status")]
    HeadlessRunStatus {
        session_id: SessionId,
        run_id: RunId,
        worker_generation: u64,
        state: haider_protocol::state::RunState,
        head_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget_exhausted: Option<haider_protocol::headless::RunBudgetExhaustedV1>,
        spec: HeadlessRunSpecV1,
    },
    #[serde(rename = "headless.run.stop")]
    HeadlessRunStop {
        session_id: SessionId,
        run_id: RunId,
        status: CancelStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_seq: Option<u64>,
    },
    #[serde(rename = "workflow.graph.state")]
    WorkflowGraphState {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<haider_protocol::graph::WorkflowGraphState>,
    },
    #[serde(rename = "workflow.graph.watch")]
    WorkflowGraphWatch {
        page: haider_protocol::graph::WorkflowGraphWatchPage,
    },
    #[serde(rename = "loom.author.draft")]
    LoomAuthorDraft {
        draft: haider_protocol::loom::LoomAuthorDraft,
    },
    #[serde(rename = "loom.author.revise")]
    LoomAuthorRevise {
        draft: haider_protocol::loom::LoomAuthorDraft,
    },
    #[serde(rename = "loom.author.confirm")]
    LoomAuthorConfirm {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirmed: Option<haider_protocol::loom::LoomAuthorConfirmed>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        errors: Vec<haider_protocol::loom::LoomAuthorValidationError>,
    },
    #[serde(rename = "loom.install.cancel")]
    LoomInstallCancel {
        receipt: TypedAgentInstallCancelReceiptWire,
    },
    #[serde(rename = "loom.archive")]
    LoomArchive { receipt: LoomArchiveReceiptWire },
    #[serde(rename = "loom.unarchive")]
    LoomUnarchive { receipt: LoomArchiveReceiptWire },
    #[serde(rename = "loom.validate")]
    LoomValidate {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        errors: Vec<haider_protocol::loom::LoomAuthorValidationError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        canonical_digest: Option<String>,
    },
    #[serde(rename = "loom.watch")]
    LoomWatch {
        watch_id: String,
        requested_after_cursor: u64,
        baseline: haider_protocol::loom::LoomRegistrySnapshot,
    },
    #[serde(rename = "checkpoint.list")]
    CheckpointList {
        page: haider_protocol::checkpoint::CheckpointListPage,
    },
    #[serde(rename = "checkpoint.undo")]
    CheckpointUndo {
        receipt: haider_protocol::checkpoint::CheckpointMutationReceipt,
    },
    #[serde(rename = "checkpoint.redo")]
    CheckpointRedo {
        receipt: haider_protocol::checkpoint::CheckpointMutationReceipt,
    },
    #[serde(rename = "checkpoint.rollback_turn")]
    CheckpointRollbackTurn {
        receipt: haider_protocol::checkpoint::CheckpointMutationReceipt,
    },
    /// Decode artifact for a method this crate does not know (tolerance
    /// discipline).
    #[serde(other)]
    Unknown,
}

/// Worker disposition returned after durable turn acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubmitDisposition {
    Started,
    Queued,
    SteerPending,
    SubturnPending,
    #[serde(other)]
    Unknown,
}

/// Result of a durable turn-cancellation command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CancelStatus {
    Accepted,
    AlreadyTerminal,
    #[serde(other)]
    Unknown,
}

/// Machine-readable recovery coordinates attached to a correlated
/// [`ResponseBody::Error`].
///
/// Tagged by `code`-matching kind so future codes can add variants without
/// breaking old clients; an unknown kind decodes as [`ErrorData::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorData {
    /// Decoded `artifact.put` bytes exceeded the hard request cap.
    ArtifactTooLarge { actual_bytes: u64, max_bytes: u64 },
    /// One attachment reference was absent from the verified CAS.
    AttachmentNotFound { index: u32, artifact: ArtifactRef },
    /// An image attachment declared a MIME outside the allowlist.
    AttachmentMimeUnsupported { index: u32, mime: String },
    /// One verified attachment exceeded its per-object cap.
    AttachmentTooLarge {
        index: u32,
        artifact: ArtifactRef,
        actual_bytes: u64,
        max_bytes: u64,
    },
    /// A verified PDF exceeded the PDF-specific byte cap.
    PdfTooLarge {
        index: u32,
        artifact: ArtifactRef,
        actual_bytes: u64,
        max_bytes: u64,
        presentation: haider_protocol::error::ErrorPresentation,
    },
    /// The parsed PDF page tree exceeded the page cap.
    PdfTooManyPages {
        index: u32,
        artifact: ArtifactRef,
        actual_pages: u32,
        max_pages: u32,
        presentation: haider_protocol::error::ErrorPresentation,
    },
    /// The PDF header/object/page tree could not be parsed safely.
    PdfMalformed {
        index: u32,
        artifact: ArtifactRef,
        presentation: haider_protocol::error::ErrorPresentation,
    },
    /// A turn carried too many attachment blocks.
    TooManyAttachments { actual_count: u32, max_count: u32 },
    /// Verified attachment bytes exceeded the aggregate turn cap.
    AttachmentsTooLarge { actual_bytes: u64, max_bytes: u64 },
    /// The selected provider explicitly lacks native or emulated vision.
    VisionUnsupported { provider: String },
    /// The client's `after_seq` is beyond the committed head
    /// ([`ERROR_CODE_CURSOR_AHEAD`]): reattach from a sequence at or below
    /// `head`.
    CursorAhead {
        /// The cursor the client asked to resume after.
        requested: u64,
        /// The greatest committed sequence the daemon holds.
        head: u64,
    },
    /// A compare-and-set command lost to an earlier resolution
    /// ([`ERROR_CODE_ALREADY_RESOLVED`]): the winning resolution is the
    /// envelope at `resolution_seq` on the event stream.
    AlreadyResolved {
        /// Sequence of the envelope recording the winning resolution.
        resolution_seq: u64,
    },
    /// A revision-fenced compare-and-set request observed a newer snapshot
    /// ([`ERROR_CODE_REVISION_CONFLICT`]).
    RevisionConflict {
        expected_revision: u64,
        current_revision: u64,
    },
    /// A volatile surface or injected input value exceeded its byte cap.
    SurfaceTextTooLarge {
        field: String,
        actual_bytes: u64,
        max_bytes: u64,
    },
    /// The provider did not serve a model catalog to the active credential.
    ProviderModelsUnavailable { provider: String, reason: String },
    /// A custom provider's pre-configuration `/v1/models` probe failed.
    /// Public coordinates only: neither a staged reference nor credential
    /// material may appear here.
    ProviderProbeFailed {
        provider: String,
        failure: ProviderProbeFailureWire,
    },
    /// A model selection named a row whose provider attribute is not
    /// creatable on this daemon ([`ERROR_CODE_PROVIDER_UNAVAILABLE`]).
    ProviderUnavailable { provider: String },
    /// A model selection named a model outside the implied provider's KNOWN
    /// discovered inventory ([`ERROR_CODE_MODEL_UNKNOWN`]).
    ModelUnknown {
        provider: String,
        model: String,
        /// Age of the live inventory consulted, in milliseconds. Absent for
        /// seeded/legacy inventories whose fetch time is unknown.
        #[serde(
            rename = "inventory_age",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        inventory_age_ms: Option<u64>,
    },
    /// An effort selection named a level outside the CURRENT pair's declared
    /// ladder ([`ERROR_CODE_EFFORT_UNSUPPORTED`]). `supported` is the exact
    /// ladder the daemon validated against — EMPTY means the pair declares
    /// no effort vocabulary at all (G3).
    EffortUnsupported {
        provider: String,
        model: String,
        effort: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        supported: Vec<String>,
    },
    /// A fast-mode enable named a pair outside the static fast gate
    /// ([`ERROR_CODE_FAST_UNSUPPORTED`]) (G3).
    FastUnsupported { provider: String, model: String },
    /// Cache-impact preflight for a live configuration change (CM3).
    CacheEpochConfirmationRequired {
        changed_fields: Vec<String>,
        invalidated_stable_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rewarm_cost_microusd: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rewarm_api_equivalent_cost_microusd: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rewarm_base_input_equivalent_tokens: Option<u64>,
        policy: String,
    },
    /// A custom-provider removal was refused. Blocking credential aliases are
    /// carried as typed data so clients never need to parse the message.
    ProviderRemoveRefused {
        provider: String,
        reason: ProviderRemoveRefusalReasonWire,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocking_aliases: Vec<String>,
    },
    /// A workflow selection digest no longer names the current revision.
    WorkflowRevisionConflict {
        expected_digest: String,
        current_digest: String,
        current_revision: u32,
    },
    /// A Loom registry save/archive fence did not match current durable truth.
    LoomRevisionConflict {
        expected: haider_protocol::loom::LoomRevisionExpectation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_rev: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_digest: Option<String>,
    },
    CheckpointConflict {
        conflict: haider_protocol::checkpoint::CheckpointConflict,
    },
    CheckpointRollbackConflict {
        conflict: haider_protocol::checkpoint::CheckpointRollbackConflict,
    },
    CheckpointBranchMismatch {
        checkpoint_id: haider_protocol::ids::CheckpointId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint_branch_id: Option<BranchId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_branch_id: Option<BranchId>,
    },
    /// Decode artifact for a data kind this crate does not know (tolerance
    /// discipline).
    #[serde(other)]
    Unknown,
}

/// Stable, secret-free failure class for custom-provider discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderProbeFailureWire {
    Unreachable,
    Unauthorized,
    NonOpenAiCompatibleBody,
    EmptyList,
    Unavailable,
    #[serde(other)]
    Unknown,
}

impl ErrorData {
    /// Returns the typed E2-E4 presentation carried by error-data variants
    /// that own one. Older/fact-only variants intentionally return `None`.
    #[must_use]
    pub fn presentation(&self) -> Option<&haider_protocol::error::ErrorPresentation> {
        match self {
            Self::PdfTooLarge { presentation, .. }
            | Self::PdfTooManyPages { presentation, .. }
            | Self::PdfMalformed { presentation, .. } => Some(presentation),
            _ => None,
        }
    }
}

/// Machine-readable reason a `provider.remove` command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderRemoveRefusalReasonWire {
    NotFound,
    ReleaseOwned,
    BlockingAccounts,
    #[serde(other)]
    Unknown,
}

/// Protocol-error wire shape, also returned by failed negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    /// Stable, machine-readable `snake_case` token. Codes are strings on the
    /// wire so an old client can carry a code it does not recognize.
    pub code: String,
    /// Human-readable detail; never load-bearing for client behavior.
    pub message: String,
    /// When `true`, the sender will close the connection after this frame.
    pub fatal: bool,
    /// Typed cross-surface presentation. Optional for negotiation errors from
    /// older peers and mandatory for daemon profile diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<haider_protocol::error::ErrorPresentation>,
    /// Durable write ids that did not commit. This out-of-band list is needed
    /// precisely when the journal cannot record its own failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_write_ids: Vec<String>,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProtocolError {}

/// Optional value submitted with a menu selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MenuInput {
    /// Free-form input for question/file-style menus.
    Text {
        /// User-entered non-secret text.
        text: String,
    },
    /// Reference to a secret previously stored through a non-journaled vault RPC.
    ///
    /// The raw secret must never appear in this wire frame.
    SecretVaultReference {
        /// Opaque vault reference resolvable by the daemon.
        vault_reference: String,
    },
}

/// One versioned logical frame shared by WebSocket and UDS transports.
///
/// # Serde tagging rationale
///
/// The JSON representation is internally tagged with a stable `kind` and a
/// top-level `"v": 1`. Internal tagging was chosen over adjacent tagging
/// because it keeps every frame one flat, inspectable object — the version,
/// the discriminant, and the fields sit side by side, which keeps golden
/// fixtures readable and lets tooling grep a transcript by `"kind"`. Adjacent
/// tagging would bury variant fields under a content key for no wire benefit.
/// Unknown object fields are intentionally ignored by Serde; an unknown
/// `kind` decodes to [`WireFrame::Unknown`] (tolerance discipline), while a
/// wrong `"v"` is rejected outright (see [`WIRE_PROTOCOL_VERSION`]).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WireFrame {
    /// First application frame, client to daemon. Authentication happens
    /// before this frame (WS) or via endpoint access (UDS), never inside it.
    Hello(Hello),
    /// Daemon reply to [`WireFrame::Hello`]; carries the negotiation outcome.
    Welcome(Welcome),
    /// Correlated operation. `request_id` is connection-scoped: the matching
    /// [`WireFrame::Response`] echoes it. It is not an idempotency key —
    /// retrying across connections requires a durable [`CommandId`].
    Request {
        request_id: RequestId,
        body: RequestBody,
    },
    /// Answer to the [`WireFrame::Request`] whose `request_id` it echoes.
    Response {
        request_id: RequestId,
        body: ResponseBody,
    },
    /// One committed envelope. `envelope.seq` is the ONLY replay cursor:
    /// there is deliberately no frame-level event ID, counter, or snapshot
    /// generation to compete with it. Delivery is at-least-once; clients drop
    /// `seq <= last_applied` and treat a gap as a signal to reattach.
    Event {
        attachment_id: AttachmentId,
        session_id: SessionId,
        envelope: RawEnvelope,
    },
    /// Replay for the attachment is complete through `high_water_seq`.
    ///
    /// This frame may REPEAT on the same attachment with strictly increasing
    /// `high_water_seq`: the daemon's internal buffering may transparently
    /// resume an attachment from durable history, replaying the gap and
    /// announcing the new head. Clients treat every occurrence identically —
    /// events deduplicate by `seq` alone (R9/R11) — and must not assume the
    /// first caught-up marker is the last.
    AttachCaughtUp {
        attachment_id: AttachmentId,
        high_water_seq: u64,
    },
    /// Changed or newly discovered session summaries for a roster watcher.
    ///
    /// Each frame carries at most 64 summaries. Larger baselines and change
    /// sets are split into independently droppable chunks.
    ///
    /// v1 deliberately does not report removed sessions. Clients that need
    /// deletion reconciliation must occasionally issue `session.list`.
    SessionRosterDelta { summaries: Vec<SessionSummary> },
    /// The account registry changed; `revision` is the newly published one.
    /// Carries no descriptors on purpose — a watcher re-reads `account.list`,
    /// so this frame can never disagree with the snapshot it announces.
    AccountsChanged { revision: u64 },
    /// Current server-published plan state for the active Haider Code account.
    /// Transport failures publish no frame and therefore cannot overwrite a
    /// client's last provider truth.
    HaiderCodePlanStatus {
        provider: String,
        account_alias: CredentialAlias,
        outcome: haider_protocol::usage::HaiderCodePlanOutcomeV1,
    },
    /// The foreground session of the resident TUI on this daemon profile.
    ///
    /// A resident TUI publishes this uncorrelated signal whenever its binding
    /// changes; the daemon validates the generation and fans it out to other
    /// clients. `None` is the explicit unbound/launcher state. Consumers must
    /// apply the signal only when `worker_generation` equals their current
    /// authoritative generation, exactly as `turn.cancel` and `menu.answer`
    /// fence stale coordinates.
    ResidentSessionBinding {
        session_id: Option<SessionId>,
        worker_generation: u64,
        /// Opaque publisher-supplied correlator for this binding. The daemon
        /// echoes it but never treats it as identity, routing, or authority.
        binding_token: Option<String>,
    },
    /// Complete latest volatile surface snapshot after one or more accepted
    /// changes. `None` means the corresponding surface is cleared.
    SessionSurfaceDelta {
        session_id: SessionId,
        input: Option<SurfaceInputWire>,
        status: Option<SurfaceStatusWire>,
    },
    /// Input operation delivered to the current input publisher. The owner
    /// applies it to its composer and republishes the resulting snapshot.
    SessionInputInjected {
        session_id: SessionId,
        op: SurfaceInjectOp,
    },
    /// Wire shape of the durable compare-and-set menu command: first
    /// committed answer wins, and `request_seq` plus `worker_generation`
    /// fence stale answers. Only the shape lives here — validation,
    /// arbitration, and the append are daemon (W3b) work.
    MenuAnswer {
        /// Optional connection-scoped correlation for the daemon's answer.
        ///
        /// The durable compare-and-set identity is, and stays, `command_id`;
        /// this field exists only so a CAS loser can be told through a
        /// [`Self::Response`] — which requires a [`RequestId`] — that it lost
        /// ([`ERROR_CODE_ALREADY_RESOLVED`]). A client that omits it accepts
        /// an uncorrelated [`Self::ProtocolError`] instead; older daemons that
        /// never sent the field keep decoding.
        request_id: Option<RequestId>,
        command_id: CommandId,
        session_id: SessionId,
        menu_id: MenuId,
        request_seq: u64,
        worker_generation: u64,
        /// Stable key from the committed menu option.
        option_key: String,
        /// Display-order index from the same committed menu version.
        option_index: u32,
        /// Optional free-form text or secret vault reference.
        input: Option<MenuInput>,
    },
    /// The daemon dropped this attachment under backpressure.
    ///
    /// `last_queued_seq` is informational server telemetry, not resume
    /// authority: queued does not mean fully applied. Under the R9 cursor law,
    /// every client reattaches using its own greatest fully applied sequence.
    Lagged {
        attachment_id: AttachmentId,
        last_queued_seq: u64,
    },
    /// The daemon entered its drain window and will stop accepting new work.
    ServerDraining {
        /// Human-readable/operator-facing drain cause.
        reason: String,
        /// Random identity of the draining daemon process.
        instance_id: String,
        /// Durable per-profile generation of the draining daemon.
        daemon_generation: u64,
        /// Absolute Unix timestamp in milliseconds.
        ///
        /// This is never a duration. At or after this instant the daemon may
        /// force remaining work to stop.
        deadline_unix_ms: u64,
    },
    /// Uncorrelated liveness probe; `nonce` is echoed verbatim by [`Self::Pong`].
    ///
    /// Ping/Pong are top-level frames per the binding protocol report. v0.1
    /// deliberately has no duplicate request-body liveness methods.
    Ping { nonce: u64 },
    /// Top-level answer to [`Self::Ping`].
    Pong { nonce: u64 },
    /// A connection-level fault; `fatal` decides whether the connection closes.
    ///
    /// Request-specific failures use [`ResponseBody::Error`] so they retain
    /// their `request_id` correlation.
    ProtocolError(ProtocolError),
    /// One typed event on a reconnectable descendant attachment. Raw child
    /// envelopes remain tagged with distinct session and agent identities in
    /// [`SessionDescendantStreamEventWire::Envelope`].
    SessionDescendantStream {
        attachment_id: AttachmentId,
        event: SessionDescendantStreamEventWire,
    },
    /// System-lane terminal repair after a descendant attachment can no
    /// longer deliver authoritatively. It identifies affected children but
    /// makes no sequence claim; the client reuses its own applied cursors.
    SessionDescendantRepairRequired {
        attachment_id: AttachmentId,
        children: Vec<DescendantIdentityWire>,
    },
    /// One replayable monitor-report revision. This is a dedicated delivery
    /// record, not a chat message and not a private mobile transport packet.
    MonitorDelivery {
        watch_id: String,
        report: MonitorDeliveryReportWire,
    },
    /// The monitor watch has scanned every session journal envelope through
    /// `high_water_cursor`, including non-report facts. Persisting this cursor
    /// avoids rescanning a report-free suffix on reconnect.
    MonitorDeliveryCaughtUp {
        watch_id: String,
        session_id: SessionId,
        high_water_cursor: u64,
    },
    /// One committed Loom registry delta on a connection-scoped watch lane.
    LoomRegistryDelta {
        watch_id: String,
        delta: haider_protocol::loom::LoomRegistryDelta,
    },
    /// The registry watcher has replayed every durable delta through this
    /// cursor. Clients persist it only after applying all preceding deltas.
    LoomRegistryCaughtUp {
        watch_id: String,
        high_water_cursor: u64,
    },
    /// Decode artifact for a frame kind this crate does not know (tolerance
    /// discipline). Never constructed for sending.
    Unknown,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireFrameRef<'a> {
    Hello(&'a Hello),
    Welcome(&'a Welcome),
    Request {
        request_id: &'a RequestId,
        body: &'a RequestBody,
    },
    Response {
        request_id: &'a RequestId,
        body: &'a ResponseBody,
    },
    Event {
        attachment_id: &'a AttachmentId,
        session_id: &'a SessionId,
        envelope: &'a RawEnvelope,
    },
    AttachCaughtUp {
        attachment_id: &'a AttachmentId,
        high_water_seq: u64,
    },
    SessionRosterDelta {
        summaries: &'a [SessionSummary],
    },
    AccountsChanged {
        revision: u64,
    },
    HaiderCodePlanStatus {
        provider: &'a str,
        account_alias: &'a CredentialAlias,
        outcome: &'a haider_protocol::usage::HaiderCodePlanOutcomeV1,
    },
    ResidentSessionBinding {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: &'a Option<SessionId>,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding_token: &'a Option<String>,
    },
    SessionSurfaceDelta {
        session_id: &'a SessionId,
        input: &'a Option<SurfaceInputWire>,
        status: &'a Option<SurfaceStatusWire>,
    },
    SessionInputInjected {
        session_id: &'a SessionId,
        op: &'a SurfaceInjectOp,
    },
    MenuAnswer {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: &'a Option<RequestId>,
        command_id: &'a CommandId,
        session_id: &'a SessionId,
        menu_id: &'a MenuId,
        request_seq: u64,
        worker_generation: u64,
        option_key: &'a str,
        option_index: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: &'a Option<MenuInput>,
    },
    Lagged {
        attachment_id: &'a AttachmentId,
        last_queued_seq: u64,
    },
    ServerDraining {
        reason: &'a str,
        instance_id: &'a str,
        daemon_generation: u64,
        deadline_unix_ms: u64,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    ProtocolError(&'a ProtocolError),
    SessionDescendantStream {
        attachment_id: &'a AttachmentId,
        event: &'a SessionDescendantStreamEventWire,
    },
    SessionDescendantRepairRequired {
        attachment_id: &'a AttachmentId,
        children: &'a [DescendantIdentityWire],
    },
    MonitorDelivery {
        watch_id: &'a str,
        report: &'a MonitorDeliveryReportWire,
    },
    MonitorDeliveryCaughtUp {
        watch_id: &'a str,
        session_id: &'a SessionId,
        high_water_cursor: u64,
    },
    LoomRegistryDelta {
        watch_id: &'a str,
        delta: &'a haider_protocol::loom::LoomRegistryDelta,
    },
    LoomRegistryCaughtUp {
        watch_id: &'a str,
        high_water_cursor: u64,
    },
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireFrameOwned {
    Hello(Hello),
    Welcome(Welcome),
    Request {
        request_id: RequestId,
        body: RequestBody,
    },
    Response {
        request_id: RequestId,
        body: ResponseBody,
    },
    Event {
        attachment_id: AttachmentId,
        session_id: SessionId,
        envelope: RawEnvelope,
    },
    AttachCaughtUp {
        attachment_id: AttachmentId,
        high_water_seq: u64,
    },
    SessionRosterDelta {
        #[serde(default)]
        summaries: Vec<SessionSummary>,
    },
    AccountsChanged {
        revision: u64,
    },
    HaiderCodePlanStatus {
        provider: String,
        account_alias: CredentialAlias,
        outcome: haider_protocol::usage::HaiderCodePlanOutcomeV1,
    },
    ResidentSessionBinding {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        worker_generation: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding_token: Option<String>,
    },
    SessionSurfaceDelta {
        session_id: SessionId,
        #[serde(default)]
        input: Option<SurfaceInputWire>,
        #[serde(default)]
        status: Option<SurfaceStatusWire>,
    },
    SessionInputInjected {
        session_id: SessionId,
        op: SurfaceInjectOp,
    },
    MenuAnswer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<RequestId>,
        command_id: CommandId,
        session_id: SessionId,
        menu_id: MenuId,
        request_seq: u64,
        worker_generation: u64,
        option_key: String,
        option_index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<MenuInput>,
    },
    Lagged {
        attachment_id: AttachmentId,
        last_queued_seq: u64,
    },
    ServerDraining {
        reason: String,
        instance_id: String,
        daemon_generation: u64,
        deadline_unix_ms: u64,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    ProtocolError(ProtocolError),
    SessionDescendantStream {
        attachment_id: AttachmentId,
        event: SessionDescendantStreamEventWire,
    },
    SessionDescendantRepairRequired {
        attachment_id: AttachmentId,
        #[serde(default)]
        children: Vec<DescendantIdentityWire>,
    },
    MonitorDelivery {
        watch_id: String,
        report: MonitorDeliveryReportWire,
    },
    MonitorDeliveryCaughtUp {
        watch_id: String,
        session_id: SessionId,
        high_water_cursor: u64,
    },
    LoomRegistryDelta {
        watch_id: String,
        delta: haider_protocol::loom::LoomRegistryDelta,
    },
    LoomRegistryCaughtUp {
        watch_id: String,
        high_water_cursor: u64,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Serialize)]
struct VersionedFrameRef<'a> {
    #[serde(rename = "v")]
    version: u32,
    #[serde(flatten)]
    frame: WireFrameRef<'a>,
}

pub(crate) struct BorrowedEventFrame<'a> {
    pub(crate) attachment_id: &'a AttachmentId,
    pub(crate) session_id: &'a SessionId,
    pub(crate) envelope: &'a RawEnvelope,
}

impl Serialize for BorrowedEventFrame<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        VersionedFrameRef {
            version: WIRE_PROTOCOL_VERSION,
            frame: WireFrameRef::Event {
                attachment_id: self.attachment_id,
                session_id: self.session_id,
                envelope: self.envelope,
            },
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
struct VersionedFrameOwned {
    #[serde(rename = "v")]
    version: u32,
    #[serde(flatten)]
    frame: WireFrameOwned,
}

impl Serialize for WireFrame {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let frame = match self {
            Self::Hello(body) => WireFrameRef::Hello(body),
            Self::Welcome(body) => WireFrameRef::Welcome(body),
            Self::Request { request_id, body } => WireFrameRef::Request { request_id, body },
            Self::Response { request_id, body } => WireFrameRef::Response { request_id, body },
            Self::Event {
                attachment_id,
                session_id,
                envelope,
            } => WireFrameRef::Event {
                attachment_id,
                session_id,
                envelope,
            },
            Self::AttachCaughtUp {
                attachment_id,
                high_water_seq,
            } => WireFrameRef::AttachCaughtUp {
                attachment_id,
                high_water_seq: *high_water_seq,
            },
            Self::SessionRosterDelta { summaries } => {
                WireFrameRef::SessionRosterDelta { summaries }
            }
            Self::AccountsChanged { revision } => WireFrameRef::AccountsChanged {
                revision: *revision,
            },
            Self::HaiderCodePlanStatus {
                provider,
                account_alias,
                outcome,
            } => WireFrameRef::HaiderCodePlanStatus {
                provider,
                account_alias,
                outcome,
            },
            Self::ResidentSessionBinding {
                session_id,
                worker_generation,
                binding_token,
            } => WireFrameRef::ResidentSessionBinding {
                session_id,
                worker_generation: *worker_generation,
                binding_token,
            },
            Self::SessionSurfaceDelta {
                session_id,
                input,
                status,
            } => WireFrameRef::SessionSurfaceDelta {
                session_id,
                input,
                status,
            },
            Self::SessionInputInjected { session_id, op } => {
                WireFrameRef::SessionInputInjected { session_id, op }
            }
            Self::MenuAnswer {
                request_id,
                command_id,
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option_key,
                option_index,
                input,
            } => WireFrameRef::MenuAnswer {
                request_id,
                command_id,
                session_id,
                menu_id,
                request_seq: *request_seq,
                worker_generation: *worker_generation,
                option_key,
                option_index: *option_index,
                input,
            },
            Self::Lagged {
                attachment_id,
                last_queued_seq,
            } => WireFrameRef::Lagged {
                attachment_id,
                last_queued_seq: *last_queued_seq,
            },
            Self::ServerDraining {
                reason,
                instance_id,
                daemon_generation,
                deadline_unix_ms,
            } => WireFrameRef::ServerDraining {
                reason,
                instance_id,
                daemon_generation: *daemon_generation,
                deadline_unix_ms: *deadline_unix_ms,
            },
            Self::Ping { nonce } => WireFrameRef::Ping { nonce: *nonce },
            Self::Pong { nonce } => WireFrameRef::Pong { nonce: *nonce },
            Self::ProtocolError(error) => WireFrameRef::ProtocolError(error),
            Self::SessionDescendantStream {
                attachment_id,
                event,
            } => WireFrameRef::SessionDescendantStream {
                attachment_id,
                event,
            },
            Self::SessionDescendantRepairRequired {
                attachment_id,
                children,
            } => WireFrameRef::SessionDescendantRepairRequired {
                attachment_id,
                children,
            },
            Self::MonitorDelivery { watch_id, report } => {
                WireFrameRef::MonitorDelivery { watch_id, report }
            }
            Self::MonitorDeliveryCaughtUp {
                watch_id,
                session_id,
                high_water_cursor,
            } => WireFrameRef::MonitorDeliveryCaughtUp {
                watch_id,
                session_id,
                high_water_cursor: *high_water_cursor,
            },
            Self::LoomRegistryDelta { watch_id, delta } => {
                WireFrameRef::LoomRegistryDelta { watch_id, delta }
            }
            Self::LoomRegistryCaughtUp {
                watch_id,
                high_water_cursor,
            } => WireFrameRef::LoomRegistryCaughtUp {
                watch_id,
                high_water_cursor: *high_water_cursor,
            },
            Self::Unknown => WireFrameRef::Unknown,
        };
        VersionedFrameRef {
            version: WIRE_PROTOCOL_VERSION,
            frame,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WireFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let versioned = VersionedFrameOwned::deserialize(deserializer)?;
        if versioned.version != WIRE_PROTOCOL_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported wire version {}; expected {}",
                versioned.version, WIRE_PROTOCOL_VERSION
            )));
        }
        Ok(match versioned.frame {
            WireFrameOwned::Hello(body) => Self::Hello(body),
            WireFrameOwned::Welcome(body) => Self::Welcome(body),
            WireFrameOwned::Request { request_id, body } => Self::Request { request_id, body },
            WireFrameOwned::Response { request_id, body } => Self::Response { request_id, body },
            WireFrameOwned::Event {
                attachment_id,
                session_id,
                envelope,
            } => Self::Event {
                attachment_id,
                session_id,
                envelope,
            },
            WireFrameOwned::AttachCaughtUp {
                attachment_id,
                high_water_seq,
            } => Self::AttachCaughtUp {
                attachment_id,
                high_water_seq,
            },
            WireFrameOwned::SessionRosterDelta { summaries } => {
                Self::SessionRosterDelta { summaries }
            }
            WireFrameOwned::AccountsChanged { revision } => Self::AccountsChanged { revision },
            WireFrameOwned::HaiderCodePlanStatus {
                provider,
                account_alias,
                outcome,
            } => Self::HaiderCodePlanStatus {
                provider,
                account_alias,
                outcome,
            },
            WireFrameOwned::ResidentSessionBinding {
                session_id,
                worker_generation,
                binding_token,
            } => Self::ResidentSessionBinding {
                session_id,
                worker_generation,
                binding_token,
            },
            WireFrameOwned::SessionSurfaceDelta {
                session_id,
                input,
                status,
            } => Self::SessionSurfaceDelta {
                session_id,
                input,
                status,
            },
            WireFrameOwned::SessionInputInjected { session_id, op } => {
                Self::SessionInputInjected { session_id, op }
            }
            WireFrameOwned::MenuAnswer {
                request_id,
                command_id,
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option_key,
                option_index,
                input,
            } => Self::MenuAnswer {
                request_id,
                command_id,
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option_key,
                option_index,
                input,
            },
            WireFrameOwned::Lagged {
                attachment_id,
                last_queued_seq,
            } => Self::Lagged {
                attachment_id,
                last_queued_seq,
            },
            WireFrameOwned::ServerDraining {
                reason,
                instance_id,
                daemon_generation,
                deadline_unix_ms,
            } => Self::ServerDraining {
                reason,
                instance_id,
                daemon_generation,
                deadline_unix_ms,
            },
            WireFrameOwned::Ping { nonce } => Self::Ping { nonce },
            WireFrameOwned::Pong { nonce } => Self::Pong { nonce },
            WireFrameOwned::ProtocolError(error) => Self::ProtocolError(error),
            WireFrameOwned::SessionDescendantStream {
                attachment_id,
                event,
            } => Self::SessionDescendantStream {
                attachment_id,
                event,
            },
            WireFrameOwned::SessionDescendantRepairRequired {
                attachment_id,
                children,
            } => Self::SessionDescendantRepairRequired {
                attachment_id,
                children,
            },
            WireFrameOwned::MonitorDelivery { watch_id, report } => {
                Self::MonitorDelivery { watch_id, report }
            }
            WireFrameOwned::MonitorDeliveryCaughtUp {
                watch_id,
                session_id,
                high_water_cursor,
            } => Self::MonitorDeliveryCaughtUp {
                watch_id,
                session_id,
                high_water_cursor,
            },
            WireFrameOwned::LoomRegistryDelta { watch_id, delta } => {
                Self::LoomRegistryDelta { watch_id, delta }
            }
            WireFrameOwned::LoomRegistryCaughtUp {
                watch_id,
                high_water_cursor,
            } => Self::LoomRegistryCaughtUp {
                watch_id,
                high_water_cursor,
            },
            WireFrameOwned::Unknown => Self::Unknown,
        })
    }
}
