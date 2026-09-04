//! The Google Antigravity adapter: a [`crate::Provider`] backed by the
//! supervised `antigravity-acp` agent instead of an HTTP endpoint.
//!
//! Google owns the OAuth. Haider never implements Google's OAuth, never holds
//! a Google token, and never touches a Code Assist HTTP endpoint; it
//! supervises Google's official agent and speaks ACP to it over stdio.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use haider_protocol::provider::{CapabilityDoc, FeatureResolve, FinishReason, StreamEvent};
use tokio::sync::mpsc;

use crate::acp::GOOGLE_ANTIGRAVITY_PROVIDER_NAME;
use crate::acp::client::{
    ACP_CANCEL_GRACE, ACP_PROMPT_IDLE_TIMEOUT, ACP_PROMPT_OPEN_TIMEOUT, AcpChildReap,
    AcpClientHandler, AcpConnection, AcpError, AcpLaunchSpec, PromptEvent, PromptStream,
    RefusingAcpClientHandler, rpc_error_is_cancellation,
};
use crate::acp::wire::{
    ACP_ERROR_AUTH_REQUIRED, AcpModelCatalog, AuthMethod, AuthMethodType, ClientInfo, ContentBlock,
    SessionConfigOption, SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate,
};
use crate::{
    Block, MessageRole, Provider, ProviderError, ProviderErrorKind, ProviderStream,
    ProviderStreamItem, TurnRequest,
};

/// The ONLY auth method id Haider will ever pass to `authenticate`.
///
/// The live 1.1.1 binary also advertises `oauth-business`, `gemini-api-key`
/// and `agent-platform`. Falling back to any of them would silently move the
/// turn onto a different Google account, an API key, or Vertex/ADC, so the
/// selection is exact and there is no fallback.
pub const ACP_OAUTH_PERSONAL_METHOD_ID: &str = "oauth-personal";

/// Maximum distinct agent-executed tool calls surfaced per turn.
///
/// Derivation. These rows are DISPLAY-ONLY, so the bound protects memory, not
/// correctness. `haider_rpc::FLEET_MAX_NODES` bounds a comparable per-response
/// fan-out at 512, and this map costs 512 * (a ~64-byte tool call id + one
/// bool + hash overhead, ~96 bytes) = ~48 KiB for an open turn. A turn that
/// exceeds it keeps streaming text; further tool rows are elided.
const ACP_MAX_TRACKED_TOOL_CALLS: usize = 512;

/// Maximum Unicode scalars retained in one server-tool preview.
///
/// Derivation. This is exactly the class of string
/// `haider_protocol::pipe::TOOL_PREVIEW_CHARS` already bounds for a cold
/// history tool row, so the same 160 scalars are used rather than a second
/// constant with the same job.
const ACP_TOOL_PREVIEW_CHARS: usize = haider_protocol::pipe::TOOL_PREVIEW_CHARS;

/// Context window advertised for the Antigravity model family.
///
/// Derivation. ACP exposes the real window only through `usage_update.size`,
/// and Antigravity never sends that update, so the number cannot be read off
/// the wire. Every model the agent offers is a `gemini-3.x` build, and this
/// crate's own Gemini adapter answers 1_048_576 for `gemini-3` prefixed models
/// (`gemini_context_limit`), so the same figure is used and stays consistent
/// with the rest of the catalog.
const ACP_CONTEXT_LIMIT: u64 = 1_048_576;

/// Provider stream queue depth, matching every other adapter in this crate.
const STREAM_CAPACITY: usize = 32;

/// How the supervised agent was reached.
#[derive(Debug, Clone)]
pub struct AntigravitySessionConfig {
    /// Working directory handed to `session/new`.
    pub cwd: String,
    /// Extra roots the agent may touch, passed verbatim as
    /// `additionalDirectories`.
    pub additional_directories: Vec<String>,
    /// The model the caller REQUESTS for this session — a request, not a
    /// decision. The authoritative catalog arrives on `session/new` as a
    /// configuration option, so this value is only the fallback recorded when
    /// the agent published no model selector to report a current value from.
    /// Antigravity's list drifts server-side and `gemini-pro-agent` is an
    /// irregular slug, so it is carried opaquely and never parsed
    /// structurally.
    pub model: String,
}

/// A `Provider` served by one supervised Antigravity ACP session.
#[derive(Debug)]
pub struct AntigravityAcpProvider {
    connection: Arc<AcpConnection>,
    session_id: String,
    models: Arc<Mutex<SessionModels>>,
}

/// The session's live model state.
///
/// Shared with the turn decoder rather than owned by the provider, because a
/// `config_option_update` arrives mid-turn and must refresh the very cache the
/// next selection reads.
#[derive(Debug, Default)]
struct SessionModels {
    /// The slug the session is actually running on. Recorded from what the
    /// agent reports, never from what Haider asked for.
    model: String,
    /// The catalog the agent published. `None` means it published no model
    /// selector at all — the ONE state that is genuinely "no catalog".
    catalog: Option<AcpModelCatalog>,
}

fn lock_models(models: &Mutex<SessionModels>) -> MutexGuard<'_, SessionModels> {
    models.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AntigravityAcpProvider {
    /// Spawns the real agent and completes the handshake.
    pub async fn launch(
        spec: &AcpLaunchSpec,
        config: &AntigravitySessionConfig,
        handler: Arc<dyn AcpClientHandler>,
    ) -> Result<Self, ProviderError> {
        let connection =
            AcpConnection::spawn(spec, handler).map_err(|error| error.into_provider_error(""))?;
        Self::handshake(connection, config).await
    }

    /// Completes the handshake over an already-connected transport. Tests use
    /// this with `tokio::io::duplex`; production reaches it through
    /// [`Self::launch`].
    pub async fn handshake(
        connection: Arc<AcpConnection>,
        config: &AntigravitySessionConfig,
    ) -> Result<Self, ProviderError> {
        let initialized = connection
            .initialize(ClientInfo {
                name: "haider".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            })
            .await
            .map_err(|error| Self::provider_error(&connection, error))?;

        // Authenticate LAZILY. A profile directory that already holds a valid
        // token needs no browser round trip, and the agent answers a
        // premature `session/new` with exactly -32000 / "Authentication
        // required", which is the documented signal to log in.
        let session = match connection
            .new_session(&config.cwd, config.additional_directories.clone())
            .await
        {
            Ok(session) => session,
            Err(AcpError::Rpc(error)) if error.code == ACP_ERROR_AUTH_REQUIRED => {
                let method = select_oauth_personal_method(&initialized.auth_methods)
                    .map_err(|error| Self::provider_error(&connection, error))?;
                connection
                    .authenticate(&method.id)
                    .await
                    .map_err(|error| Self::provider_error(&connection, error))?;
                connection
                    .new_session(&config.cwd, config.additional_directories.clone())
                    .await
                    .map_err(|error| Self::provider_error(&connection, error))?
            }
            Err(error) => return Err(Self::provider_error(&connection, error)),
        };

        // The model catalog is a session CONFIGURATION OPTION, resolved out of
        // `configOptions`. Until this point in the exchange there is no model
        // list to be had at all: ACP publishes no `models` field.
        let catalog = AcpModelCatalog::resolve(&session.config_options);
        // What the session runs on right now is the selector's own current
        // value. The requested model is only a fallback for reporting, because
        // a request is not yet a selection.
        let model = catalog
            .as_ref()
            .and_then(|catalog| catalog.current_value.clone())
            .unwrap_or_else(|| config.model.clone());

        tracing::debug!(
            target: "haider.provider.acp",
            agent = initialized
                .agent_info
                .as_ref()
                .map_or("unknown", |info| info.name.as_str()),
            agent_version = initialized
                .agent_info
                .as_ref()
                .and_then(|info| info.version.as_deref())
                .unwrap_or("unknown"),
            load_session = initialized
                .agent_capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.load_session),
            image_prompts = initialized
                .agent_capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.prompt_capabilities.image),
            offered_models = catalog.as_ref().map_or(0, |catalog| catalog.models.len()),
            "opened an ACP session with the supervised Antigravity agent"
        );
        Ok(Self {
            connection,
            session_id: session.session_id,
            models: Arc::new(Mutex::new(SessionModels { model, catalog })),
        })
    }

    /// Convenience constructor for the default, capability-refusing handler.
    pub fn refusing_handler() -> Arc<dyn AcpClientHandler> {
        Arc::new(RefusingAcpClientHandler)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The model slug this session is RUNNING on: the selector's current
    /// value, refreshed by every selection and every `config_option_update`.
    /// Antigravity's catalog drifts server-side and `gemini-pro-agent` is an
    /// irregular slug, so the value is carried opaquely and never parsed
    /// structurally.
    pub fn model(&self) -> String {
        lock_models(&self.models).model.clone()
    }

    /// The catalog the agent published for this session, or `None` when it
    /// published no model selector.
    ///
    /// A snapshot: the cache behind it is refreshed by a selection and by a
    /// mid-turn `config_option_update`, and the caller's model policy runs
    /// against one consistent view rather than a live borrow.
    pub fn model_catalog(&self) -> Option<AcpModelCatalog> {
        lock_models(&self.models).catalog.clone()
    }

    /// Puts this session on `model_id`.
    ///
    /// The catalog's `currentValue` is what the session is ALREADY on, so a
    /// model that matches it is recorded without a round trip; anything else
    /// is written with `session/set_config_option`, and the agent's answer —
    /// the full option set — replaces the cache.
    ///
    /// Membership is the CALLER's policy, not this function's: the daemon
    /// decides which offered model a session may run on, and re-deciding it
    /// here would be a second, divergent policy.
    pub async fn select_model(&self, model_id: &str) -> Result<(), ProviderError> {
        let (config_id, already_current) = {
            let state = lock_models(&self.models);
            let Some(catalog) = state.catalog.as_ref() else {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "the supervised Antigravity agent published no model selector for this \
                     session, so no model can be set on it",
                ));
            };
            (
                catalog.config_id.clone(),
                catalog.current_value.as_deref() == Some(model_id),
            )
        };
        if already_current {
            lock_models(&self.models).model = model_id.to_owned();
            return Ok(());
        }
        let response = self
            .connection
            .set_config_option(&self.session_id, &config_id, model_id)
            .await
            .map_err(|error| Self::provider_error(&self.connection, error))?;
        let refreshed = AcpModelCatalog::resolve(&response.config_options);
        let mut state = lock_models(&self.models);
        state.model = refreshed
            .as_ref()
            .and_then(|catalog| catalog.current_value.clone())
            .unwrap_or_else(|| model_id.to_owned());
        // A response that resolves no selector leaves the previous catalog
        // standing: the schema lets the field be absent, and dropping a
        // catalog the session demonstrably has would refuse the next
        // selection on a session that can serve one.
        if refreshed.is_some() {
            state.catalog = refreshed;
        }
        Ok(())
    }

    pub fn connection(&self) -> &Arc<AcpConnection> {
        &self.connection
    }

    /// Cancels the active turn. Cancellation is an OUTCOME: the turn's stream
    /// still ends with one `Finish { Cancelled }`, never an error.
    pub async fn cancel_active_turn(&self) -> Result<(), ProviderError> {
        self.connection
            .cancel(&self.session_id)
            .await
            .map_err(|error| Self::provider_error(&self.connection, error))
    }

    /// Cancels, then terminates, force-kills and reaps the supervised child.
    pub async fn shutdown(&self) -> AcpChildReap {
        self.connection.shutdown(Some(&self.session_id)).await
    }

    /// The adapter's capability declaration.
    ///
    /// Every field is a property of the ADAPTER, not of a live session, so a
    /// caller can answer a capability question without paying the agent's
    /// measured ~14.75 s cold start and ~225 MiB child. [`Provider::capabilities`]
    /// is this function.
    #[must_use]
    pub fn declared_capabilities() -> CapabilityDoc {
        CapabilityDoc {
            provider: GOOGLE_ANTIGRAVITY_PROVIDER_NAME.into(),
            // The agent executes its OWN tools. Haider never dispatches a tool
            // for this provider, so local tool calling — parallel or serial,
            // streamed args or not — is not a feature this adapter serves.
            parallel_tools: FeatureResolve::Unsupported,
            streaming_tool_args: FeatureResolve::Unsupported,
            // `promptCapabilities.image` is true on the live agent, but this
            // slice sends text blocks only, so images are not yet declared.
            vision: FeatureResolve::Unsupported,
            pdf_documents: FeatureResolve::Unsupported,
            // `agent_thought_chunk` is a first-class session update.
            thinking_visible: FeatureResolve::Native,
            context_limit: ACP_CONTEXT_LIMIT,
        }
    }

    fn provider_error(connection: &Arc<AcpConnection>, error: AcpError) -> ProviderError {
        error.into_provider_error(&connection.stderr_tail())
    }
}

/// Selects the personal-Google OAuth method, or fails naming what the agent
/// actually advertised.
///
/// A `terminal`-typed method is rejected even under the right id: the schema
/// says such a method is run as a separate interactive process and is NOT
/// passed to `authenticate`.
pub fn select_oauth_personal_method(methods: &[AuthMethod]) -> Result<&AuthMethod, AcpError> {
    methods
        .iter()
        .find(|method| {
            method.id == ACP_OAUTH_PERSONAL_METHOD_ID && method.method_type == AuthMethodType::Agent
        })
        .ok_or_else(|| AcpError::AuthMethodUnavailable {
            advertised: methods.iter().map(|method| method.id.clone()).collect(),
        })
}

#[async_trait]
impl Provider for AntigravityAcpProvider {
    /// The supervised agent reaches Google over the network, so a confirmed
    /// missing OS default route is authoritative for it exactly as it is for
    /// the direct Gemini adapter.
    fn trusts_default_route_absence(&self) -> bool {
        true
    }

    /// Haider holds no Google credential for this provider at all: the child
    /// owns its own OAuth material under `$GEMINI_HOME`, and nothing token-
    /// shaped ever crosses the ACP wire.
    fn credential_surface(&self) -> crate::ProviderCredentialSurface {
        crate::ProviderCredentialSurface::Opaque
    }

    fn usage_lane_dimensions(&self) -> haider_protocol::provider::UsageLaneDimensions {
        haider_protocol::provider::UsageLaneDimensions {
            api_family: Some("acp_antigravity".into()),
            effort: None,
            speed: None,
        }
    }

    async fn capabilities(&self) -> CapabilityDoc {
        Self::declared_capabilities()
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let prompt = prompt_blocks(&request)?;
        let stream = self
            .connection
            .open_prompt(&self.session_id, prompt)
            .await
            .map_err(|error| Self::provider_error(&self.connection, error))?;
        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let models = Arc::clone(&self.models);
        let producer = tokio::spawn(async move {
            drive_turn(stream, sender, models).await;
        });
        Ok(ProviderStream::owned(receiver, producer))
    }
}

/// Renders the ACP prompt for one turn.
///
/// Only the NEWEST user message is sent. The ACP session owns its own
/// conversation history — `session/new` opens it and every `session/prompt`
/// appends to it — so replaying Haider's transcript would duplicate every
/// earlier turn inside the agent's context.
///
/// Haider's system prompt is deliberately NOT sent: the supervised agent
/// carries its own harness instructions, and there is no ACP field that would
/// let a client replace them rather than fight them turn after turn.
fn prompt_blocks(request: &TurnRequest) -> Result<Vec<ContentBlock>, ProviderError> {
    let message = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "an Antigravity turn requires at least one user message",
            )
        })?;
    let blocks: Vec<ContentBlock> = message
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => {
                let text = text.to_owned_string();
                (!text.is_empty()).then_some(ContentBlock::Text { text })
            }
            _ => None,
        })
        .collect();
    if blocks.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "an Antigravity turn requires at least one non-empty text block",
        ));
    }
    Ok(blocks)
}

/// Sends `session/cancel` if the turn's producer task is dropped before it
/// reached a terminal — which is exactly what `ProviderStream::drop` does when
/// a caller abandons a turn. Disarmed once a terminal has been emitted.
struct CancelOnDrop {
    connection: Arc<AcpConnection>,
    session_id: String,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The drop may run while the task is being aborted, so the cancel must
        // be detached. Outside a runtime there is nothing to detach onto and
        // the child's `kill_on_drop` guard is the remaining backstop.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let connection = Arc::clone(&self.connection);
        let session_id = std::mem::take(&mut self.session_id);
        handle.spawn(async move {
            let _ = haider_platform::bounded_wait(
                "acp cancel on drop",
                ACP_CANCEL_GRACE,
                connection.cancel(&session_id),
            )
            .await;
        });
    }
}

/// Drives one open turn to exactly ONE terminal event.
///
/// Every exit path from this function emits either one `Finish` or one
/// `ProviderError` and then returns, so a turn can never end with zero
/// terminals or two.
async fn drive_turn(
    mut stream: PromptStream,
    sender: mpsc::Sender<ProviderStreamItem>,
    models: Arc<Mutex<SessionModels>>,
) {
    let mut guard = CancelOnDrop {
        connection: Arc::clone(stream.connection()),
        session_id: stream.session_id().to_owned(),
        armed: true,
    };
    let mut decoder = AcpTurnDecoder::new(models);
    let mut opened = false;
    loop {
        let budget = if opened {
            ACP_PROMPT_IDLE_TIMEOUT
        } else {
            ACP_PROMPT_OPEN_TIMEOUT
        };
        let event = match haider_platform::bounded_wait("acp session/prompt", budget, stream.recv())
            .await
        {
            haider_platform::BoundedWait::Completed(Some(event)) => event,
            // The session channel closed without a terminal: the child's
            // stdout ended mid-turn.
            haider_platform::BoundedWait::Completed(None) => {
                guard.armed = false;
                emit_error(&stream, &sender, AcpError::Closed).await;
                return;
            }
            // The guard stays ARMED on a timeout: Haider is abandoning a turn
            // the agent may still be working on, so the drop must tell it to
            // stop rather than leave it running against a dead consumer.
            haider_platform::BoundedWait::TimedOut(timeout) => {
                emit_error(
                    &stream,
                    &sender,
                    AcpError::Timeout {
                        operation: timeout.operation(),
                        limit: timeout.limit(),
                    },
                )
                .await;
                return;
            }
        };
        opened = true;
        match event {
            PromptEvent::Update(update) => {
                for stream_event in decoder.decode(update) {
                    if sender.send(Ok(stream_event)).await.is_err() {
                        return;
                    }
                }
            }
            PromptEvent::Finished(reason) => {
                guard.armed = false;
                let _ = sender
                    .send(Ok(StreamEvent::Finish {
                        reason: finish_reason(reason),
                    }))
                    .await;
                return;
            }
            PromptEvent::Failed(AcpError::Rpc(error)) if rpc_error_is_cancellation(&error) => {
                // Cancellation is an OUTCOME, never an error.
                guard.armed = false;
                let _ = sender
                    .send(Ok(StreamEvent::Finish {
                        reason: FinishReason::Cancelled,
                    }))
                    .await;
                return;
            }
            PromptEvent::Failed(error) => {
                guard.armed = false;
                emit_error(&stream, &sender, error).await;
                return;
            }
        }
    }
}

async fn emit_error(
    stream: &PromptStream,
    sender: &mpsc::Sender<ProviderStreamItem>,
    error: AcpError,
) {
    let tail = stream.connection().stderr_tail();
    let _ = sender.send(Err(error.into_provider_error(&tail))).await;
}

/// Maps one ACP stop reason onto Haider's terminal vocabulary.
pub fn finish_reason(reason: StopReason) -> FinishReason {
    match reason {
        StopReason::EndTurn => FinishReason::EndTurn,
        StopReason::MaxTokens => FinishReason::MaxTokens,
        // The agent exhausted its own per-turn request budget, so the answer
        // is TRUNCATED, not complete. `MaxTokens` is Haider's only
        // hit-a-hard-budget outcome; reporting `EndTurn` would claim a
        // finished answer that does not exist.
        StopReason::MaxTurnRequests => FinishReason::MaxTokens,
        StopReason::Refusal => FinishReason::Refusal,
        StopReason::Cancelled => FinishReason::Cancelled,
    }
}

/// Per-turn mapping state.
#[derive(Debug)]
struct AcpTurnDecoder {
    /// Tool call id -> whether a result row has already been emitted.
    tool_calls: HashMap<String, bool>,
    /// The session's model cache, refreshed in place by a mid-turn
    /// `config_option_update`.
    models: Arc<Mutex<SessionModels>>,
}

impl AcpTurnDecoder {
    fn new(models: Arc<Mutex<SessionModels>>) -> Self {
        Self {
            tool_calls: HashMap::new(),
            models,
        }
    }

    fn decode(&mut self, update: SessionUpdate) -> Vec<StreamEvent> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => chunk
                .content
                .text()
                .filter(|text| !text.is_empty())
                .map(|text| {
                    vec![StreamEvent::TextDelta {
                        text: text.to_owned().into(),
                    }]
                })
                .unwrap_or_default(),
            SessionUpdate::AgentThoughtChunk(chunk) => chunk
                .content
                .text()
                .filter(|text| !text.is_empty())
                .map(|text| {
                    vec![StreamEvent::ReasoningDelta {
                        text: text.to_owned().into(),
                    }]
                })
                .unwrap_or_default(),
            SessionUpdate::ToolCall(call) => self.decode_tool_call(&call),
            SessionUpdate::ToolCallUpdate(update) => self.decode_tool_call_update(&update),
            // `usage_update` is CONTEXT-WINDOW OCCUPANCY (`used` tokens in a
            // window of `size`), not billing and not subscription quota, and
            // Antigravity never sends it anyway. Synthesizing a `Usage` from
            // it would put a fabricated number into tokenomics, so it is
            // deliberately dropped for accounting and only traced.
            SessionUpdate::UsageUpdate(usage) => {
                tracing::debug!(
                    target: "haider.provider.acp",
                    used = usage.used,
                    size = usage.size,
                    cost_reported = usage.cost.is_some(),
                    "ignored an ACP context-window occupancy update for accounting"
                );
                Vec::new()
            }
            // Configuration options can change mid-session, and the model
            // catalog is one of them. Nothing is rendered, but the cache the
            // next selection reads is brought up to date.
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.refresh_catalog(&update.config_options);
                Vec::new()
            }
            // `user_message_chunk` is the agent echoing Haider's own prompt.
            // Everything else here is agent UI state Haider does not render.
            SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::Plan
            | SessionUpdate::AvailableCommandsUpdate
            | SessionUpdate::CurrentModeUpdate
            | SessionUpdate::SessionInfoUpdate
            | SessionUpdate::Other => Vec::new(),
        }
    }

    /// A `config_option_update` carries the FULL option set with current
    /// values, so a resolvable catalog REPLACES the cached one.
    ///
    /// An update that resolves no model selector is left alone: it is some
    /// other option's state, and treating it as a withdrawal would refuse the
    /// next selection on a session whose catalog never went away.
    fn refresh_catalog(&self, options: &[SessionConfigOption]) {
        let Some(catalog) = AcpModelCatalog::resolve(options) else {
            return;
        };
        let mut state = lock_models(&self.models);
        if let Some(current) = catalog.current_value.clone() {
            state.model = current;
        }
        state.catalog = Some(catalog);
    }

    /// AGENT-EXECUTED tool calls surface as display-only server-tool rows.
    ///
    /// They must NEVER become `ToolCallStart`/`ToolCallArgsDelta`/
    /// `ToolCallEnd`: those events feed Haider's LOCAL dispatch loop, and the
    /// agent has already run the tool, so re-dispatching would execute it a
    /// second time.
    fn decode_tool_call(&mut self, call: &ToolCall) -> Vec<StreamEvent> {
        if !self.track(&call.tool_call_id) {
            return Vec::new();
        }
        let mut events = vec![StreamEvent::ServerToolUse {
            call_id: call.tool_call_id.clone(),
            name: display_name(&call.title, call.kind.as_str()),
            args: call.raw_input.clone().unwrap_or(serde_json::Value::Null),
        }];
        if call.status.is_terminal() {
            events.extend(self.finish_tool_call(
                &call.tool_call_id,
                call.status,
                &call.content,
                call.raw_output.as_ref(),
            ));
        }
        events
    }

    fn decode_tool_call_update(&mut self, update: &ToolCallUpdate) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.tool_calls.contains_key(&update.tool_call_id) {
            // An update for a call whose opening frame was lost or elided
            // still opens a display row so the result has something to close.
            if !self.track(&update.tool_call_id) {
                return events;
            }
            events.push(StreamEvent::ServerToolUse {
                call_id: update.tool_call_id.clone(),
                name: display_name(
                    update.title.as_deref().unwrap_or_default(),
                    update.kind.unwrap_or_default().as_str(),
                ),
                args: update.raw_input.clone().unwrap_or(serde_json::Value::Null),
            });
        }
        if update.status.is_some_and(ToolCallStatus::is_terminal) {
            let status = update.status.unwrap_or_default();
            events.extend(self.finish_tool_call(
                &update.tool_call_id,
                status,
                &update.content,
                update.raw_output.as_ref(),
            ));
        }
        events
    }

    /// Registers a tool call id, or reports that the per-turn display bound is
    /// already reached.
    fn track(&mut self, tool_call_id: &str) -> bool {
        if self.tool_calls.contains_key(tool_call_id) {
            return true;
        }
        if self.tool_calls.len() >= ACP_MAX_TRACKED_TOOL_CALLS {
            return false;
        }
        self.tool_calls.insert(tool_call_id.to_owned(), false);
        true
    }

    /// Emits at most ONE result row per tool call id.
    fn finish_tool_call(
        &mut self,
        tool_call_id: &str,
        status: ToolCallStatus,
        content: &[ToolCallContent],
        raw_output: Option<&serde_json::Value>,
    ) -> Vec<StreamEvent> {
        let Some(emitted) = self.tool_calls.get_mut(tool_call_id) else {
            return Vec::new();
        };
        if *emitted {
            return Vec::new();
        }
        *emitted = true;
        vec![StreamEvent::ServerToolResult {
            call_id: tool_call_id.to_owned(),
            preview: tool_preview(status, content, raw_output),
            is_error: status.is_error(),
        }]
    }
}

/// A tool row's display name. ACP's `ToolCall` carries no tool identifier, so
/// the human `title` is the most specific thing available and `kind` is the
/// fallback when the agent sent none.
fn display_name(title: &str, kind: &str) -> String {
    if title.is_empty() {
        kind.to_owned()
    } else {
        title.to_owned()
    }
}

/// Builds the bounded, display-only result preview.
///
/// Prefers the tool call's own text content; falls back to a compact rendering
/// of `rawOutput`, which is arbitrary agent-authored JSON and therefore never
/// leaves this bounded display path.
fn tool_preview(
    status: ToolCallStatus,
    content: &[ToolCallContent],
    raw_output: Option<&serde_json::Value>,
) -> String {
    let text = content
        .iter()
        .filter_map(|entry| entry.content.as_ref().and_then(ContentBlock::text))
        .find(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| raw_output.map(serde_json::Value::to_string))
        .unwrap_or_else(|| {
            if status.is_error() {
                "failed".to_owned()
            } else {
                "completed".to_owned()
            }
        });
    text.chars().take(ACP_TOOL_PREVIEW_CHARS).collect()
}
