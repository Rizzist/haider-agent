//! Google Gemini GenerateContent API adapter.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use haider_accounts::SecretHandle;
use haider_protocol::ids::{ArtifactRef, CredentialAlias};
use haider_protocol::provider::{
    Block, CapabilityDoc, FeatureResolve, FinishReason, StreamEvent, Usage, UsageSource,
};
use haider_protocol::tool::AttachmentBlock;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use tokio::sync::mpsc;

use crate::origin::{FixedDnsResolver, FixedOriginGuard, SystemFixedDnsResolver};
use crate::{
    MessageRole, Provider, ProviderError, ProviderErrorKind, ProviderStream, ProviderStreamItem,
    TurnRequest, Utf8Assembler,
};

pub const GEMINI_PROVIDER_NAME: &str = "gemini";
pub const GEMINI_API_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const GEMINI_MODELS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const GEMINI_API_HOST: &str = "generativelanguage.googleapis.com";
const STREAM_CAPACITY: usize = 32;
const ERROR_BODY_LIMIT: usize = 64 * 1024;
const OPAQUE_KIND: &str = "signed_part";
const CALL_ID_PREFIX: &str = "gemini-call-";
const TRANSPORT_CONFIG: GeminiTransportConfig = GeminiTransportConfig {
    retry_policy: GeminiRetryPolicy::Never,
    connect_timeout: Duration::from_secs(10),
    response_open_timeout: Duration::from_secs(30),
    chunk_idle_timeout: Duration::from_secs(90),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiRetryPolicy {
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeminiTransportConfig {
    pub retry_policy: GeminiRetryPolicy,
    pub connect_timeout: Duration,
    pub response_open_timeout: Duration,
    pub chunk_idle_timeout: Duration,
}

/// Raw response returned only to the explicitly gated fixture-promotion path.
#[derive(Debug)]
pub struct GeminiCapture {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

/// Gemini adapter backed by one already-resolved API key.
#[derive(Debug)]
pub struct GeminiProvider {
    client: reqwest::Client,
    credential: SecretHandle,
    account: Option<CredentialAlias>,
    model: String,
    api_url: String,
    fixed_origin_guard: Arc<FixedOriginGuard>,
}

impl GeminiProvider {
    pub fn new(credential: SecretHandle, model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::new_with_dns_resolver(credential, model, Arc::new(SystemFixedDnsResolver))
    }

    pub(crate) fn new_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        let model = model.into();
        let api_url = gemini_stream_endpoint(&model)?;
        let fixed_origin_guard =
            Arc::new(FixedOriginGuard::new(&api_url, GEMINI_API_HOST, resolver)?);
        let transport = Self::transport_config();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(match transport.retry_policy {
                GeminiRetryPolicy::Never => reqwest::retry::never(),
            })
            .connect_timeout(transport.connect_timeout)
            .dns_resolver(Arc::clone(&fixed_origin_guard))
            .build()
            .map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::Internal,
                    format!("could not construct Gemini HTTP client: {error}"),
                )
            })?;
        Ok(Self {
            client,
            credential,
            account: None,
            model,
            api_url,
            fixed_origin_guard,
        })
    }

    #[must_use]
    pub const fn transport_config() -> GeminiTransportConfig {
        TRANSPORT_CONFIG
    }

    #[must_use]
    pub fn with_account(mut self, account: CredentialAlias) -> Self {
        self.account = Some(account);
        self
    }

    #[cfg(test)]
    pub(crate) fn client_debug(&self) -> String {
        format!("{:?}", self.client)
    }

    #[cfg(test)]
    pub(crate) fn stall_fixed_connection_resolution(&self) {
        self.fixed_origin_guard.stall_connection_resolution();
    }

    #[cfg(test)]
    pub(crate) fn fixed_connection_resolution_count(&self) -> usize {
        self.fixed_origin_guard.connection_resolution_count()
    }

    #[cfg(test)]
    pub(crate) async fn execute_request_for_test(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client.execute(request).await
    }

    pub fn request_payload(
        &self,
        request: &TurnRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        self.validate_model(request)?;
        gemini_request_json(request)
    }

    pub async fn capture_response(
        &self,
        request: &TurnRequest,
    ) -> Result<GeminiCapture, ProviderError> {
        let response = self.send_request(request).await?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await.map_err(transport_error)?.to_vec();
        Ok(GeminiCapture {
            status,
            retry_after,
            body,
        })
    }

    fn validate_model(&self, request: &TurnRequest) -> Result<(), ProviderError> {
        if request.model == self.model {
            Ok(())
        } else {
            Err(invalid_request(format!(
                "Gemini provider selected model `{}`, but turn requested `{}`",
                self.model, request.model
            )))
        }
    }

    fn api_key_header(&self) -> Result<HeaderValue, ProviderError> {
        let mut value = HeaderValue::from_bytes(self.credential.expose_secret()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "resolved Gemini credential is not a valid HTTP header value",
            )
        })?;
        value.set_sensitive(true);
        Ok(value)
    }

    pub(crate) async fn request(
        &self,
        payload: &serde_json::Value,
    ) -> Result<reqwest::Request, ProviderError> {
        // The guard validates and resolves before credential material is
        // placed on a request. `alt=sse` is a fixed non-routing query added
        // only after the exact HTTPS path has passed the gate.
        self.fixed_origin_guard
            .validate_endpoint(&self.api_url)
            .await?;
        let request_url = format!("{}?alt=sse", self.api_url);
        self.client
            .post(request_url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .header("x-goog-api-key", self.api_key_header()?)
            .json(payload)
            .build()
            .map_err(transport_error)
    }

    async fn send_request(
        &self,
        request: &TurnRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let payload = self.request_payload(request)?;
        let request = self.request(&payload).await?;
        tokio::time::timeout(
            Self::transport_config().response_open_timeout,
            self.client.execute(request),
        )
        .await
        .map_err(|_| response_open_timeout_error(Self::transport_config().response_open_timeout))?
        .map_err(transport_error)
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn credential_surface(&self) -> crate::ProviderCredentialSurface {
        crate::ProviderCredentialSurface::ApiKey
    }

    async fn capabilities(&self) -> CapabilityDoc {
        let context_limit = gemini_context_limit(&self.model);
        CapabilityDoc {
            provider: GEMINI_PROVIDER_NAME.into(),
            parallel_tools: FeatureResolve::Native,
            // GenerateContent returns each function-call argument object as a
            // whole; the adapter normalizes it to one complete args delta.
            streaming_tool_args: FeatureResolve::ExplicitlyEmulated,
            vision: FeatureResolve::Native,
            thinking_visible: if self.model.starts_with("gemini-2.5")
                || self.model.starts_with("gemini-3")
            {
                FeatureResolve::Native
            } else {
                FeatureResolve::Unsupported
            },
            context_limit,
        }
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let next_call_index = next_synthesized_call_index(&request)?;
        let response = self.send_request(&request).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = read_error_body_bounded(response).await?;
            return Err(replay_gemini_http_error(
                status,
                retry_after.as_deref(),
                &body,
            ));
        }

        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let account = self.account.clone();
        let chunk_idle_timeout = Self::transport_config().chunk_idle_timeout;
        let producer = tokio::spawn(async move {
            stream_sse_source(
                response,
                account,
                next_call_index,
                sender,
                chunk_idle_timeout,
            )
            .await;
        });
        Ok(ProviderStream::owned(receiver, producer))
    }
}

fn gemini_stream_endpoint(model: &str) -> Result<String, ProviderError> {
    if model.is_empty()
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_request(
            "Gemini model must be a nonempty model slug without path or query syntax",
        ));
    }
    let mut url = reqwest::Url::parse(GEMINI_API_BASE_URL)
        .map_err(|_| internal("Gemini API base URL is invalid"))?;
    url.path_segments_mut()
        .map_err(|_| internal("Gemini API base URL cannot carry path segments"))?
        .pop_if_empty()
        .push("models")
        .push(&format!("{model}:streamGenerateContent"));
    Ok(url.into())
}

fn gemini_context_limit(model: &str) -> u64 {
    if model.starts_with("gemini-3")
        || model.starts_with("gemini-2.5")
        || model.starts_with("gemini-2.0")
        || model.starts_with("gemini-1.5")
    {
        1_048_576
    } else {
        128_000
    }
}

fn next_synthesized_call_index(request: &TurnRequest) -> Result<u64, ProviderError> {
    let mut count = 0_u64;
    let mut greatest = None;
    for message in &request.messages {
        for block in &message.blocks {
            if let Block::ToolCall { call_id, .. } = block {
                count = count.saturating_add(1);
                if let Some(suffix) = call_id.strip_prefix(CALL_ID_PREFIX)
                    && let Ok(index) = u64::from_str_radix(suffix, 16)
                {
                    greatest = Some(greatest.map_or(index, |current: u64| current.max(index)));
                }
            }
            if let Block::ProviderOpaque { provider, data } = block
                && provider == GEMINI_PROVIDER_NAME
                && let OpaqueReplay::FunctionCall { call_id, .. } = parse_gemini_opaque(data)?
                && let Some(suffix) = call_id.strip_prefix(CALL_ID_PREFIX)
                && let Ok(index) = u64::from_str_radix(suffix, 16)
            {
                greatest = Some(greatest.map_or(index, |current: u64| current.max(index)));
            }
        }
    }
    Ok(greatest.map_or(count, |index| index.saturating_add(1)))
}

fn synthesized_call_id(index: u64) -> String {
    format!("{CALL_ID_PREFIX}{index:016x}")
}

#[derive(Debug, Clone)]
struct FunctionIdentity {
    name: String,
    args: serde_json::Value,
}

type CallNameIndex = HashMap<String, String>;
type OpaqueCallIndex = HashMap<String, FunctionIdentity>;
type ToolCallIndex = (CallNameIndex, OpaqueCallIndex);

fn gemini_request_json(request: &TurnRequest) -> Result<serde_json::Value, ProviderError> {
    let attachments = attachment_index(request)?;
    let (tool_names, opaque_calls) = tool_call_index(request)?;
    let mut contents = Vec::<serde_json::Value>::new();
    let mut pending_signed_text = VecDeque::<String>::new();

    for message in &request.messages {
        let role = match message.role {
            MessageRole::Assistant => "model",
            MessageRole::User | MessageRole::Tool => "user",
        };
        if role == "user" && !pending_signed_text.is_empty() {
            return Err(invalid_request(
                "Gemini signed text part is missing its normalized assistant text block",
            ));
        }
        let mut parts = Vec::new();
        for block in &message.blocks {
            match block {
                Block::Text { text } if message.role != MessageRole::Tool => {
                    if message.role == MessageRole::Assistant
                        && let Some(expected) = pending_signed_text.front()
                    {
                        if expected != text {
                            return Err(invalid_request(
                                "Gemini signed text part disagrees with normalized history",
                            ));
                        }
                        pending_signed_text.pop_front();
                    } else {
                        parts.push(serde_json::json!({"text": text}));
                    }
                }
                Block::Text { .. } => {
                    return Err(invalid_request(
                        "Gemini tool messages cannot contain plain text blocks",
                    ));
                }
                Block::Reasoning { .. } => {
                    return Err(invalid_request(
                        "normalized reasoning summaries cannot be replayed on the Gemini wire",
                    ));
                }
                Block::ToolCall {
                    call_id,
                    name,
                    args,
                } if message.role == MessageRole::Assistant => {
                    if let Some(native) = opaque_calls.get(call_id) {
                        if native.name != *name || native.args != *args {
                            return Err(invalid_request(
                                "Gemini signed function call disagrees with normalized history",
                            ));
                        }
                    } else {
                        parts.push(serde_json::json!({
                            "functionCall": {"name": name, "args": args}
                        }));
                    }
                }
                Block::ToolCall { .. } => {
                    return Err(invalid_request(
                        "Gemini function calls are only valid in assistant messages",
                    ));
                }
                Block::ToolResult {
                    call_id,
                    preview,
                    truncated,
                } if matches!(message.role, MessageRole::User | MessageRole::Tool) => {
                    let name = tool_names.get(call_id).ok_or_else(|| {
                        invalid_request(format!(
                            "Gemini tool result references unknown call id `{call_id}`"
                        ))
                    })?;
                    parts.push(serde_json::json!({
                        "functionResponse": {
                            "name": name,
                            "response": {
                                "result": preview,
                                "truncated": truncated,
                            }
                        }
                    }));
                }
                Block::ToolResult { .. } => {
                    return Err(invalid_request(
                        "Gemini function responses are only valid in user/tool messages",
                    ));
                }
                Block::Attachment(AttachmentBlock::Image { artifact, mime, .. })
                    if message.role == MessageRole::User =>
                {
                    let data = resolved_attachment(&attachments, artifact)?;
                    parts.push(serde_json::json!({
                        "inlineData": {"mimeType": mime, "data": data}
                    }));
                }
                Block::Attachment(AttachmentBlock::Image { .. }) => {
                    return Err(invalid_request(
                        "Gemini image inputs are only valid in user messages",
                    ));
                }
                Block::Attachment(AttachmentBlock::PastedText { artifact, .. }) => {
                    return Err(invalid_request(format!(
                        "pasted-text attachment `{artifact}` was not resolved by the prompt compiler"
                    )));
                }
                Block::Attachment(AttachmentBlock::File { artifact, .. }) => {
                    return Err(invalid_request(format!(
                        "file attachment `{artifact}` was not resolved by the prompt compiler"
                    )));
                }
                Block::Attachment(AttachmentBlock::Skill { name, .. }) => {
                    return Err(invalid_request(format!(
                        "skill attachment `{name}` was not resolved by the prompt compiler"
                    )));
                }
                Block::ProviderOpaque { provider, data }
                    if provider == GEMINI_PROVIDER_NAME
                        && message.role == MessageRole::Assistant =>
                {
                    match parse_gemini_opaque(data)? {
                        OpaqueReplay::FunctionCall { part, .. }
                        | OpaqueReplay::Thought { part } => parts.push(part),
                        OpaqueReplay::Text { text, part } => {
                            parts.push(part);
                            if !text.is_empty() {
                                pending_signed_text.push_back(text);
                            }
                        }
                    }
                }
                Block::ProviderOpaque { provider, .. } if provider == GEMINI_PROVIDER_NAME => {
                    return Err(invalid_request(
                        "Gemini provider-opaque blocks are only valid in assistant messages",
                    ));
                }
                Block::ProviderOpaque { provider, .. } => {
                    return Err(invalid_request(format!(
                        "provider-opaque block for `{provider}` cannot be sent to Gemini"
                    )));
                }
            }
        }
        append_content(&mut contents, role, parts)?;
    }
    if !pending_signed_text.is_empty() {
        return Err(invalid_request(
            "Gemini signed text part is missing its normalized assistant text block",
        ));
    }

    let declarations = request
        .tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    let mut payload = serde_json::json!({
        "contents": contents,
        "generationConfig": {"maxOutputTokens": request.max_tokens},
    });
    let object = payload
        .as_object_mut()
        .ok_or_else(|| internal("Gemini request payload was not an object"))?;
    if let Some(system) = &request.system_prompt {
        object.insert(
            "system_instruction".into(),
            serde_json::json!({"parts": [{"text": system}]}),
        );
    }
    if !declarations.is_empty() {
        object.insert(
            "tools".into(),
            serde_json::json!([{"functionDeclarations": declarations}]),
        );
    }
    Ok(payload)
}

fn append_content(
    contents: &mut Vec<serde_json::Value>,
    role: &str,
    parts: Vec<serde_json::Value>,
) -> Result<(), ProviderError> {
    if parts.is_empty() {
        return Ok(());
    }
    if let Some(previous) = contents.last_mut()
        && previous.get("role").and_then(serde_json::Value::as_str) == Some(role)
    {
        let previous_parts = previous
            .get_mut("parts")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| internal("Gemini content parts were not an array"))?;
        previous_parts.extend(parts);
    } else {
        contents.push(serde_json::json!({"role": role, "parts": parts}));
    }
    Ok(())
}

fn attachment_index(request: &TurnRequest) -> Result<HashMap<&str, &str>, ProviderError> {
    let mut attachments = HashMap::new();
    for attachment in &request.attachments {
        if attachments
            .insert(
                attachment.artifact.as_str(),
                attachment.data_base64.as_str(),
            )
            .is_some()
        {
            return Err(invalid_request(format!(
                "attachment `{}` was resolved more than once",
                attachment.artifact
            )));
        }
    }
    Ok(attachments)
}

fn resolved_attachment<'a>(
    attachments: &'a HashMap<&str, &str>,
    artifact: &ArtifactRef,
) -> Result<&'a str, ProviderError> {
    attachments.get(artifact.as_str()).copied().ok_or_else(|| {
        invalid_request(format!(
            "image attachment `{artifact}` has no resolved provider bytes"
        ))
    })
}

fn tool_call_index(request: &TurnRequest) -> Result<ToolCallIndex, ProviderError> {
    let mut names = HashMap::<String, String>::new();
    let mut opaque = HashMap::<String, FunctionIdentity>::new();
    for message in &request.messages {
        for block in &message.blocks {
            match block {
                Block::ToolCall {
                    call_id,
                    name,
                    args,
                } => {
                    insert_call_name(&mut names, call_id, name)?;
                    if let Some(native) = opaque.get(call_id)
                        && (native.name != *name || native.args != *args)
                    {
                        return Err(invalid_request(
                            "Gemini signed function call disagrees with normalized history",
                        ));
                    }
                }
                Block::ProviderOpaque { provider, data } if provider == GEMINI_PROVIDER_NAME => {
                    if let OpaqueReplay::FunctionCall {
                        call_id,
                        name,
                        args,
                        ..
                    } = parse_gemini_opaque(data)?
                    {
                        insert_call_name(&mut names, &call_id, &name)?;
                        if let Some(existing) = opaque.insert(
                            call_id,
                            FunctionIdentity {
                                name: name.clone(),
                                args: args.clone(),
                            },
                        ) && (existing.name != name || existing.args != args)
                        {
                            return Err(invalid_request(
                                "Gemini history reuses a call id for different signed calls",
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok((names, opaque))
}

fn insert_call_name(
    names: &mut HashMap<String, String>,
    call_id: &str,
    name: &str,
) -> Result<(), ProviderError> {
    if let Some(existing) = names.insert(call_id.to_owned(), name.to_owned())
        && existing != name
    {
        return Err(invalid_request(format!(
            "Gemini history reuses call id `{call_id}` for different functions"
        )));
    }
    Ok(())
}

enum OpaqueReplay {
    FunctionCall {
        call_id: String,
        name: String,
        args: serde_json::Value,
        part: serde_json::Value,
    },
    Text {
        text: String,
        part: serde_json::Value,
    },
    Thought {
        part: serde_json::Value,
    },
}

fn parse_gemini_opaque(data: &serde_json::Value) -> Result<OpaqueReplay, ProviderError> {
    let object = data.as_object().ok_or_else(|| {
        invalid_request("Gemini provider-opaque continuation must be a JSON object")
    })?;
    if object.get("kind").and_then(serde_json::Value::as_str) != Some(OPAQUE_KIND) {
        return Err(invalid_request(
            "Gemini provider-opaque continuation has an unsupported kind",
        ));
    }
    let part = object
        .get("part")
        .cloned()
        .ok_or_else(|| invalid_request("Gemini provider-opaque continuation has no native part"))?;
    let part_object = part.as_object().ok_or_else(|| {
        invalid_request("Gemini provider-opaque native part must be a JSON object")
    })?;
    if part_object
        .get("thoughtSignature")
        .and_then(serde_json::Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(invalid_request(
            "Gemini provider-opaque native part has no thought signature",
        ));
    }
    if let Some(function) = part_object
        .get("functionCall")
        .and_then(serde_json::Value::as_object)
    {
        let call_id = object
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                invalid_request("Gemini signed function-call continuation has no call id")
            })?;
        let name = function
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid_request("Gemini signed function call has no name"))?;
        let args = function
            .get("args")
            .cloned()
            .ok_or_else(|| invalid_request("Gemini signed function call has no args"))?;
        return Ok(OpaqueReplay::FunctionCall {
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            args,
            part,
        });
    }
    if let Some(text) = part_object.get("text").and_then(serde_json::Value::as_str) {
        if part_object
            .get("thought")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            Ok(OpaqueReplay::Thought { part })
        } else {
            Ok(OpaqueReplay::Text {
                text: text.to_owned(),
                part,
            })
        }
    } else {
        Ok(OpaqueReplay::Thought { part })
    }
}

pub(crate) trait GeminiSseChunkSource {
    async fn next_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError>;
}

impl GeminiSseChunkSource for reqwest::Response {
    async fn next_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError> {
        self.chunk().await.map_err(transport_error)
    }
}

pub(crate) async fn stream_sse_source<S: GeminiSseChunkSource>(
    mut source: S,
    account: Option<CredentialAlias>,
    next_call_index: u64,
    sender: mpsc::Sender<ProviderStreamItem>,
    chunk_idle_timeout: Duration,
) {
    let mut decoder = GeminiDecoder::new(account, next_call_index);
    loop {
        let chunk = match tokio::time::timeout(chunk_idle_timeout, source.next_chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => {
                send_items(&sender, decoder.finish()).await;
                return;
            }
            Ok(Err(error)) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
            Err(_) => {
                let _ = sender
                    .send(Err(stream_idle_error(chunk_idle_timeout)))
                    .await;
                return;
            }
        };
        let items = decoder.push(chunk.as_ref());
        if !send_items(&sender, items).await || decoder.terminal {
            return;
        }
    }
}

async fn send_items(
    sender: &mpsc::Sender<ProviderStreamItem>,
    items: Vec<ProviderStreamItem>,
) -> bool {
    for item in items {
        if sender.send(item).await.is_err() {
            return false;
        }
    }
    true
}

#[derive(Debug)]
struct SseFrame {
    data: String,
}

#[derive(Debug, Default)]
struct SseFramer {
    utf8: Utf8Assembler,
    line_buffer: String,
    data_lines: Vec<String>,
}

impl SseFramer {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, ProviderError> {
        let decoded = self.utf8.push(bytes)?;
        let mut frames = Vec::new();
        for text in decoded {
            self.line_buffer.push_str(&text);
            while let Some(newline) = self.line_buffer.find('\n') {
                let mut line = self.line_buffer.drain(..=newline).collect::<String>();
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
                if let Some(frame) = self.accept_line(&line) {
                    frames.push(frame);
                }
            }
        }
        Ok(frames)
    }

    fn finish(&mut self) -> Result<Vec<SseFrame>, ProviderError> {
        if self.utf8.has_pending() {
            return Err(malformed("Gemini SSE stream ended inside a UTF-8 scalar"));
        }
        let mut frames = Vec::new();
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            if let Some(frame) = self.accept_line(line.trim_end_matches('\r')) {
                frames.push(frame);
            }
        }
        if let Some(frame) = self.dispatch_frame() {
            frames.push(frame);
        }
        Ok(frames)
    }

    fn accept_line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            return self.dispatch_frame();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').map_or((line, ""), |(field, value)| {
            (field, value.strip_prefix(' ').unwrap_or(value))
        });
        if field == "data" {
            self.data_lines.push(value.to_owned());
        }
        None
    }

    fn dispatch_frame(&mut self) -> Option<SseFrame> {
        if self.data_lines.is_empty() {
            return None;
        }
        let frame = SseFrame {
            data: self.data_lines.join("\n"),
        };
        self.data_lines.clear();
        Some(frame)
    }
}

#[derive(Debug)]
pub(crate) struct GeminiDecoder {
    framer: SseFramer,
    account: Option<CredentialAlias>,
    next_call_index: u64,
    saw_tool: bool,
    saw_refusal: bool,
    terminal: bool,
}

impl GeminiDecoder {
    pub(crate) fn new(account: Option<CredentialAlias>, next_call_index: u64) -> Self {
        Self {
            framer: SseFramer::default(),
            account,
            next_call_index,
            saw_tool: false,
            saw_refusal: false,
            terminal: false,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        let frames = match self.framer.push(bytes) {
            Ok(frames) => frames,
            Err(error) => return self.fail(error),
        };
        self.accept_frames(frames)
    }

    pub(crate) fn finish(&mut self) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        let frames = match self.framer.finish() {
            Ok(frames) => frames,
            Err(error) => return self.fail(error),
        };
        let mut items = self.accept_frames(frames);
        if !self.terminal {
            items.push(Err(malformed(
                "Gemini SSE stream ended before a terminal finish reason",
            )));
            self.terminal = true;
        }
        items
    }

    fn accept_frames(&mut self, frames: Vec<SseFrame>) -> Vec<ProviderStreamItem> {
        let mut output = Vec::new();
        for frame in frames {
            let items = match self.dispatch(frame) {
                Ok(items) => items.into_iter().map(Ok).collect::<Vec<_>>(),
                Err(error) => vec![Err(error)],
            };
            let terminal = items
                .iter()
                .any(|item| matches!(item, Err(_) | Ok(StreamEvent::Finish { .. })));
            output.extend(items);
            if terminal {
                self.terminal = true;
                break;
            }
        }
        output
    }

    fn dispatch(&mut self, frame: SseFrame) -> Result<Vec<StreamEvent>, ProviderError> {
        let value: serde_json::Value = serde_json::from_str(&frame.data)
            .map_err(|error| malformed(format!("Gemini SSE data is not valid JSON: {error}")))?;
        if value
            .get("promptFeedback")
            .and_then(|feedback| feedback.get("blockReason"))
            .and_then(serde_json::Value::as_str)
            .filter(|reason| *reason != "BLOCK_REASON_UNSPECIFIED")
            .is_some()
        {
            self.saw_refusal = true;
            return Ok(vec![
                safety_refusal_delta(),
                StreamEvent::Finish {
                    reason: FinishReason::Refusal,
                },
            ]);
        }

        let mut events = Vec::new();
        if let Some(candidate) = value
            .get("candidates")
            .and_then(serde_json::Value::as_array)
            .and_then(|candidates| candidates.first())
            && let Some(parts) = candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(serde_json::Value::as_array)
        {
            for part in parts {
                events.extend(self.part_events(part)?);
            }
        }
        if let Some(usage) = value.get("usageMetadata") {
            events.push(StreamEvent::UsageUpdate(gemini_usage(
                usage,
                self.account.clone(),
            )?));
        }
        if let Some(reason) = value
            .get("candidates")
            .and_then(serde_json::Value::as_array)
            .and_then(|candidates| candidates.first())
            .and_then(|candidate| candidate.get("finishReason"))
            .and_then(serde_json::Value::as_str)
        {
            let finish = match reason {
                "STOP" => {
                    if self.saw_tool {
                        FinishReason::ToolUse
                    } else {
                        FinishReason::EndTurn
                    }
                }
                "MAX_TOKENS" => FinishReason::MaxTokens,
                "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII"
                | "IMAGE_SAFETY" => {
                    if !self.saw_refusal {
                        events.push(safety_refusal_delta());
                        self.saw_refusal = true;
                    }
                    FinishReason::Refusal
                }
                "OTHER"
                | "LANGUAGE"
                | "MALFORMED_FUNCTION_CALL"
                | "UNEXPECTED_TOOL_CALL"
                | "NO_IMAGE"
                | "IMAGE_PROHIBITED_CONTENT" => FinishReason::Error,
                _ => {
                    return Err(malformed(format!(
                        "Gemini response used unknown finish reason `{reason}`"
                    )));
                }
            };
            events.push(StreamEvent::Finish { reason: finish });
        }
        Ok(events)
    }

    fn part_events(&mut self, part: &serde_json::Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let object = part
            .as_object()
            .ok_or_else(|| malformed("Gemini response part is not a JSON object"))?;
        let signature = object
            .get("thoughtSignature")
            .and_then(serde_json::Value::as_str)
            .filter(|signature| !signature.is_empty());
        if let Some(function) = object
            .get("functionCall")
            .and_then(serde_json::Value::as_object)
        {
            let name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| malformed("Gemini function call has no name"))?;
            let args = function
                .get("args")
                .cloned()
                .ok_or_else(|| malformed("Gemini function call has no args"))?;
            if !args.is_object() {
                return Err(malformed("Gemini function-call args are not an object"));
            }
            let call_id = synthesized_call_id(self.next_call_index);
            self.next_call_index = self.next_call_index.saturating_add(1);
            self.saw_tool = true;
            let mut events = Vec::new();
            if signature.is_some() {
                events.push(StreamEvent::ProviderOpaque {
                    provider: GEMINI_PROVIDER_NAME.into(),
                    data: serde_json::json!({
                        "kind": OPAQUE_KIND,
                        "call_id": call_id,
                        "part": part,
                    }),
                });
            }
            events.push(StreamEvent::ToolCallStart {
                call_id: call_id.clone(),
                name: name.to_owned(),
            });
            events.push(StreamEvent::ToolCallArgsDelta {
                call_id: call_id.clone(),
                args_fragment: serde_json::to_string(&args).map_err(|error| {
                    malformed(format!(
                        "Gemini function-call args could not be encoded: {error}"
                    ))
                })?,
            });
            events.push(StreamEvent::ToolCallEnd { call_id });
            return Ok(events);
        }
        if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
            let mut events = Vec::new();
            if signature.is_some() {
                events.push(StreamEvent::ProviderOpaque {
                    provider: GEMINI_PROVIDER_NAME.into(),
                    data: serde_json::json!({"kind": OPAQUE_KIND, "part": part}),
                });
            }
            if !text.is_empty() {
                if object
                    .get("thought")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    events.push(StreamEvent::ReasoningDelta { text: text.into() });
                } else {
                    events.push(StreamEvent::TextDelta { text: text.into() });
                }
            }
            return Ok(events);
        }
        if signature.is_some() {
            return Ok(vec![StreamEvent::ProviderOpaque {
                provider: GEMINI_PROVIDER_NAME.into(),
                data: serde_json::json!({"kind": OPAQUE_KIND, "part": part}),
            }]);
        }
        Ok(Vec::new())
    }

    fn fail(&mut self, error: ProviderError) -> Vec<ProviderStreamItem> {
        self.terminal = true;
        vec![Err(error)]
    }
}

fn safety_refusal_delta() -> StreamEvent {
    StreamEvent::RefusalDelta {
        text: "Gemini blocked the response for safety reasons.".into(),
    }
}

fn gemini_usage(
    value: &serde_json::Value,
    account: Option<CredentialAlias>,
) -> Result<Usage, ProviderError> {
    let read = |field: &str| -> Result<u64, ProviderError> {
        value.get(field).map_or(Ok(0), |value| {
            value
                .as_u64()
                .ok_or_else(|| malformed(format!("Gemini usage field `{field}` is not an integer")))
        })
    };
    Ok(Usage {
        input: read("promptTokenCount")?,
        output: read("candidatesTokenCount")?,
        reasoning: read("thoughtsTokenCount")?,
        cached: read("cachedContentTokenCount")?,
        source: UsageSource::ProviderReported,
        account,
        accounts: Vec::new(),
    })
}

/// Replay captured data-only SSE through the same 7-byte incremental path.
/// Replays a captured stream with the SAME call-index continuation the
/// production path derives from the request history — the seam that makes
/// sparse/replayed call indices testable (a dense one-call history is
/// DEGENERATE: `count == greatest + 1` there, so only a sparse history can
/// distinguish continuation from restart).
pub fn replay_gemini_sse_for_request(
    request: &TurnRequest,
    bytes: &[u8],
) -> Result<Vec<ProviderStreamItem>, ProviderError> {
    let next_call_index = next_synthesized_call_index(request)?;
    let mut decoder = GeminiDecoder::new(None, next_call_index);
    let mut items = Vec::new();
    for chunk in bytes.chunks(7) {
        items.extend(decoder.push(chunk));
        if decoder.terminal {
            return Ok(items);
        }
    }
    items.extend(decoder.finish());
    Ok(items)
}

pub fn replay_gemini_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    let mut decoder = GeminiDecoder::new(None, 0);
    let mut items = Vec::new();
    for chunk in bytes.chunks(7) {
        items.extend(decoder.push(chunk));
        if decoder.terminal {
            return items;
        }
    }
    items.extend(decoder.finish());
    items
}

/// Replay a captured non-success response through the live typed classifier.
#[must_use]
pub fn replay_gemini_http_error(
    status: u16,
    retry_after: Option<&str>,
    body: &[u8],
) -> ProviderError {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let api_error = parsed.as_ref().and_then(|value| value.get("error"));
    let wire_status = api_error
        .and_then(|error| error.get("status"))
        .and_then(serde_json::Value::as_str);
    let detail = api_error
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());
    let context_exceeded = status == 400
        && wire_status == Some("INVALID_ARGUMENT")
        && detail.is_some_and(is_context_prose);
    let overloaded_prose = detail.is_some_and(|message| {
        let lower = message.to_ascii_lowercase();
        lower.contains("overload") || lower.contains("unavailable")
    });
    let kind = match status {
        401 => ProviderErrorKind::Authentication,
        403 => ProviderErrorKind::PermissionDenied,
        429 => ProviderErrorKind::RateLimited,
        503 => ProviderErrorKind::Overloaded,
        _ if context_exceeded => ProviderErrorKind::ContextExceeded,
        _ if overloaded_prose => ProviderErrorKind::Overloaded,
        500..=599 => ProviderErrorKind::Transport,
        _ => match wire_status {
            Some("UNAUTHENTICATED") => ProviderErrorKind::Authentication,
            Some("PERMISSION_DENIED") => ProviderErrorKind::PermissionDenied,
            Some("RESOURCE_EXHAUSTED") => ProviderErrorKind::RateLimited,
            Some("UNAVAILABLE") => ProviderErrorKind::Overloaded,
            _ => ProviderErrorKind::InvalidRequest,
        },
    };
    if let Some(detail) = detail {
        let bounded = detail
            .chars()
            .filter(|character| !character.is_control())
            .take(200)
            .collect::<String>();
        eprintln!("gemini error detail (log-only): {bounded}");
    }
    let retry_after_ms = if matches!(kind, ProviderErrorKind::RateLimited) {
        parse_retry_info(api_error).or_else(|| parse_retry_after(retry_after))
    } else {
        None
    };
    ProviderError::new(
        kind,
        format!("Gemini HTTP {status} returned {}", provider_kind_name(kind)),
    )
    .with_retry_after_ms(retry_after_ms)
}

fn is_context_prose(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "token count",
        "input token",
        "too many tokens",
        "context window",
        "context length",
        "maximum number of tokens",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn parse_retry_info(error: Option<&serde_json::Value>) -> Option<u64> {
    let details = error?.get("details")?.as_array()?;
    details.iter().find_map(|detail| {
        let kind = detail.get("@type")?.as_str()?;
        kind.ends_with("google.rpc.RetryInfo")
            .then(|| detail.get("retryDelay")?.as_str())
            .flatten()
            .and_then(parse_protobuf_duration_ms)
    })
}

pub(crate) fn parse_protobuf_duration_ms(value: &str) -> Option<u64> {
    let seconds = value.strip_suffix('s')?;
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let whole = whole.parse::<u64>().ok()?;
    let fraction_digits =
        fraction
            .bytes()
            .take(3)
            .try_fold((0_u64, 0_usize), |(number, count), byte| {
                if byte.is_ascii_digit() {
                    Some((number * 10 + u64::from(byte - b'0'), count + 1))
                } else {
                    None
                }
            })?;
    if fraction.bytes().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    let millis = match fraction_digits.1 {
        0 => 0,
        1 => fraction_digits.0 * 100,
        2 => fraction_digits.0 * 10,
        _ => fraction_digits.0,
    };
    whole.checked_mul(1_000)?.checked_add(millis)
}

fn parse_retry_after(value: Option<&str>) -> Option<u64> {
    let value = value?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return seconds.checked_mul(1_000);
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    let duration = retry_at
        .duration_since(SystemTime::now())
        .unwrap_or_default();
    u64::try_from(duration.as_millis()).ok()
}

async fn read_error_body_bounded(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        let remaining = ERROR_BODY_LIMIT.saturating_sub(body.len());
        if remaining == 0 {
            break;
        }
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn provider_kind_name(kind: ProviderErrorKind) -> &'static str {
    match kind {
        ProviderErrorKind::Authentication => "authentication failure",
        ProviderErrorKind::PermissionDenied => "permission denial",
        ProviderErrorKind::RateLimited => "rate limit",
        ProviderErrorKind::Overloaded => "overload",
        ProviderErrorKind::ContextExceeded => "context overflow",
        ProviderErrorKind::InvalidRequest => "invalid request",
        ProviderErrorKind::Transport => "transport failure",
        ProviderErrorKind::MalformedFrame => "malformed frame",
        ProviderErrorKind::InvalidUtf8 => "invalid UTF-8",
        ProviderErrorKind::Internal => "internal failure",
    }
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!("Gemini HTTP transport failed: {error}"),
    )
}

fn response_open_timeout_error(timeout: Duration) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "Gemini response did not open within {} seconds",
            timeout.as_secs()
        ),
    )
}

fn stream_idle_error(timeout: Duration) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "Gemini SSE stream received no data for {} seconds",
            timeout.as_secs()
        ),
    )
}

fn invalid_request(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

fn malformed(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedFrame, message)
}

fn internal(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Internal, message)
}
