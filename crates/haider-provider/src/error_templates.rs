//! Known provider error messages that may be published on every surface
//! (durable presentation, RunFailed message, masked export, headless/SDK
//! output, lockdown, model-visible failure text).
//!
//! Provider prose is untrusted account data. Instead of trying to scrub
//! names out of arbitrary prose, a message is published only when it matches
//! one template below WHOLE and ANCHORED (leading/trailing whitespace aside):
//! a message with any extra unrecognised text does not match. Only typed
//! slots vary, and every slot value is validated by type:
//!
//! | slot           | accepted value                                           | rendered as          |
//! |----------------|----------------------------------------------------------|----------------------|
//! | `{model}`      | equals the model id Haider requested (or a seeded catalog id) | verbatim        |
//! | `{int}`        | 1–12 ASCII digits, optionally comma-grouped (`40,000`)   | verbatim             |
//! | `{num}`        | decimal number                                           | verbatim             |
//! | `{param}`      | dotted path: closed-list names, numeric indices <= 4 digits | verbatim          |
//! | `{url}`        | http(s) URL whose host is in the exact public-host allowlist | `scheme://host`  |
//! | `{duration}`   | provider retry delay such as `2s`, `1.5s`, `6m0s`, `20ms` | verbatim            |
//! | `{rate_unit}`  | closed set of OpenAI rate-limit units                     | verbatim             |
//! | `{request_id}` | equals the captured request-id header; else `safe_request_id`, <= 64 B | verbatim |
//! | `{api_version}`| `v1`, `v1beta`, `v1alpha`, `v1beta1`                      | verbatim             |
//! | `{method}`     | closed set of Gemini RPC method names                     | verbatim             |
//! | `{tool_call_id}` | a tool-call id present in this request                 | verbatim             |
//! | `{account_id}` | org/project/user id or UUID (account data)               | `[REDACTED]`         |
//! | `{api_key}`    | a (masked) API key echo                                   | `[REDACTED]`         |
//!
//! Slot values must be corroborated by what Haider itself sent or captured
//! ([`SlotEvidence`]); without evidence (for example a bare replay) a
//! template with a `{model}` or `{tool_call_id}` slot does not match.
//!
//! Anything that matches no template is "unknown": shareable surfaces show
//! the provider-class default explanation plus "details withheld", and the
//! raw text (credentials redacted) is kept only in the owner-local
//! `provider_raw_detail` field (see `haider_protocol::error`).
//!
//! Provenance: each entry names the provider and the documented or observed
//! error it reproduces. Add entries only for exact provider wording.

// Static, test-exercised template patterns must fail loudly if edited into
// invalid regexes; silently skipping one would change publication policy.
#![allow(clippy::expect_used)]

use regex::Regex;
use std::sync::LazyLock;

const REDACTED: &str = "[REDACTED]";

/// One known provider message. `text` is literal except for `{slot}` tokens.
struct Template {
    /// Provider(s) and the error the wording comes from.
    #[allow(dead_code)]
    provenance: &'static str,
    text: &'static str,
}

const fn template(provenance: &'static str, text: &'static str) -> Template {
    Template { provenance, text }
}

/// The template table. Order only matters for overlapping templates (the
/// first match wins); all current entries are mutually exclusive.
const TEMPLATES: &[Template] = &[
    // ---- Capacity / overload -------------------------------------------
    template("Anthropic overloaded_error (HTTP 529)", "Overloaded"),
    template(
        "RFC 9110 standard reason phrase (proxy/gateway body)",
        "Service Unavailable",
    ),
    template(
        "RFC 9110 standard reason phrase (proxy/gateway body)",
        "Bad Gateway",
    ),
    template(
        "RFC 9110 standard reason phrase (proxy/gateway body)",
        "Gateway Timeout",
    ),
    template(
        "RFC 9110 standard 500 reason phrase (proxy/gateway body)",
        "Internal Server Error",
    ),
    template("Anthropic api_error", "Internal server error"),
    template(
        "Gemini UNAVAILABLE (HTTP 503)",
        "The model is overloaded. Please try again later.",
    ),
    template(
        "Gemini UNAVAILABLE (HTTP 503)",
        "The service is currently unavailable.",
    ),
    template("Gemini INTERNAL (HTTP 500)", "Internal error encountered."),
    template(
        "Gemini INTERNAL (HTTP 500)",
        "An internal error has occurred. Please retry or report in https://developers.generativeai.google/guide/troubleshooting",
    ),
    template(
        "OpenAI 503 overload",
        "That model is currently overloaded with other requests. You can retry your request, or contact us through our help center at help.openai.com if the error persists.",
    ),
    template(
        "OpenAI 503 overload with request id",
        "That model is currently overloaded with other requests. You can retry your request, or contact us through our help center at help.openai.com if the error persists. (Please include the request ID {request_id} in your message.)",
    ),
    template(
        "OpenAI 500 server_error",
        "The server had an error while processing your request. Sorry about that!",
    ),
    template(
        "OpenAI 500 server_error",
        "The server had an error processing your request. Sorry about that! You can retry your request, or contact us through our help center at help.openai.com if you keep seeing this error.",
    ),
    template(
        "OpenAI 500 server_error with request id",
        "The server had an error processing your request. Sorry about that! You can retry your request, or contact us through our help center at help.openai.com if you keep seeing this error. (Please include the request ID {request_id} in your email.)",
    ),
    template(
        "OpenAI Responses server_error",
        "An error occurred while processing your request. You can retry your request, or contact us through our help center at help.openai.com if the error persists. Please include the request ID {request_id} in your message.",
    ),
    template("DeepSeek 503", "Server overloaded, please try again later."),
    template(
        "OpenAI Responses/Codex raw SSE error frame (overload)",
        "The service is overloaded. Please try again later.",
    ),
    // ---- Rate limits ---------------------------------------------------------
    template(
        "RFC 6585 standard 429 reason phrase (proxy/gateway body)",
        "Too Many Requests",
    ),
    template("DeepSeek 429", "Rate Limit Reached"),
    template(
        "OpenAI 429 rate_limit_exceeded",
        "Rate limit reached for {model} in organization {account_id} on {rate_unit}: Limit {int}, Used {int}, Requested {int}. Please try again in {duration}.",
    ),
    template(
        "OpenAI 429 rate_limit_exceeded with docs link",
        "Rate limit reached for {model} in organization {account_id} on {rate_unit}: Limit {int}, Used {int}, Requested {int}. Please try again in {duration}. Visit {url} to learn more.",
    ),
    template(
        "OpenAI 429 rate_limit_exceeded (project scoped)",
        "Rate limit reached for {model} in project {account_id} on {rate_unit}: Limit {int}, Used {int}, Requested {int}. Please try again in {duration}. Visit {url} to learn more.",
    ),
    template(
        "Anthropic 429 rate_limit_error (output tokens)",
        "This request would exceed the rate limit for your organization ({account_id}) of {int} output tokens per minute. For details, refer to: {url}. You can see the response headers for current usage. Please reduce the prompt length or the maximum tokens requested, or try again later. You may also contact sales at https://www.anthropic.com/contact-sales to discuss your options for a rate limit increase.",
    ),
    template(
        "Anthropic 429 rate_limit_error (input tokens)",
        "This request would exceed the rate limit for your organization ({account_id}) of {int} input tokens per minute. For details, refer to: {url}. You can see the response headers for current usage. Please reduce the prompt length or the maximum tokens requested, or try again later. You may also contact sales at https://www.anthropic.com/contact-sales to discuss your options for a rate limit increase.",
    ),
    template(
        "Anthropic 429 rate_limit_error (requests)",
        "Number of request tokens has exceeded your per-minute rate limit ({url}); see the response headers for current usage. Please reduce the prompt length or the maximum tokens requested, or try again later. You may also contact sales at https://www.anthropic.com/contact-sales to discuss your options for a rate limit increase.",
    ),
    template(
        "Anthropic 429 rate_limit_error (concurrency)",
        "Number of concurrent connections has exceeded your rate limit. Please try again later or contact sales at https://www.anthropic.com/contact-sales to discuss your options for a rate limit increase.",
    ),
    template(
        "Gemini RESOURCE_EXHAUSTED (HTTP 429)",
        "Resource has been exhausted (e.g. check quota).",
    ),
    template(
        "Codex backend usage_limit_reached",
        "The usage limit has been reached",
    ),
    // ---- Quota / billing -----------------------------------------------------
    template(
        "OpenAI 429 insufficient_quota",
        "You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: {url}.",
    ),
    template(
        "Gemini 429 quota",
        "You exceeded your current quota, please check your plan and billing details. For more information on this error, head to: {url}.",
    ),
    template(
        "Anthropic 400 credit balance",
        "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.",
    ),
    template("DeepSeek 402", "Insufficient Balance"),
    template(
        "OpenAI 429 billing_hard_limit_reached",
        "Billing hard limit has been reached",
    ),
    template(
        "OpenAI 429 billing_hard_limit_reached",
        "Billing hard limit has been reached.",
    ),
    // ---- Access / account ---------------------------------------------------
    template(
        "Anthropic 403 permission_error (model access)",
        "Your organization does not have access to this model.",
    ),
    template(
        "Anthropic 403 permission_error (model access)",
        "Your organization does not have access to this model. Please contact your administrator.",
    ),
    template(
        "Anthropic 403 permission_error (Claude access)",
        "Your organization does not have access to Claude. Please login again or contact your administrator.",
    ),
    template(
        "Anthropic 403 permission_error",
        "Your API key does not have permission to use the specified resource.",
    ),
    template(
        "Anthropic / OpenAI 403 organization disabled",
        "This organization has been disabled.",
    ),
    template(
        "OpenAI 401 account_deactivated",
        "Your account is not active, please check your billing details on our website.",
    ),
    template(
        "OpenAI 403 organization verification",
        "Your organization must be verified to stream this model.",
    ),
    template(
        "OpenAI 403 organization verification",
        "Your organization must be verified to stream this model. Please go to: {url} and click on Verify Organization. If you just verified, it can take up to 15 minutes for access to propagate.",
    ),
    template(
        "OpenAI 403 organization verification",
        "Your organization must be verified to use the model `{model}`. Please go to: {url} and click on Verify Organization. If you just verified, it can take up to 15 minutes for access to propagate.",
    ),
    template(
        "OpenAI 401 no organization",
        "You must be a member of an organization to use the API. Please contact us through our help center at help.openai.com.",
    ),
    template(
        "OpenAI 403 project model access",
        "Project `{account_id}` does not have access to model `{model}`",
    ),
    template(
        "Gemini PERMISSION_DENIED (HTTP 403)",
        "The caller does not have permission",
    ),
    template(
        "Gemini PERMISSION_DENIED (HTTP 403)",
        "The caller does not have permission.",
    ),
    template(
        "Gemini FAILED_PRECONDITION (HTTP 400) location",
        "User location is not supported for the API use.",
    ),
    template(
        "Gemini FAILED_PRECONDITION (HTTP 400) location",
        "User location is not supported for the API use without a billing account linked.",
    ),
    template(
        "RFC 9110 standard reason phrase (proxy/gateway body)",
        "Forbidden",
    ),
    template(
        "RFC 9110 standard reason phrase (proxy/gateway body)",
        "Unauthorized",
    ),
    // ---- Authentication --------------------------------------------------------
    template("Anthropic 401 authentication_error", "invalid x-api-key"),
    template(
        "Anthropic 401 OAuth",
        "OAuth token has expired. Please obtain a new token or refresh your existing token.",
    ),
    template(
        "Anthropic 401 OAuth",
        "OAuth authentication is currently not supported.",
    ),
    template(
        "Anthropic 400 subscription credential scope",
        "This credential is only authorized for use with Claude Code and cannot be used for other API requests.",
    ),
    template(
        "OpenAI 401 invalid_api_key",
        "Incorrect API key provided: {api_key}. You can find your API key at {url}.",
    ),
    template(
        "OpenAI 401 missing key",
        "You didn't provide an API key. You need to provide your API key in an Authorization header using Bearer auth (i.e. Authorization: Bearer YOUR_KEY), or as the password field (with blank username) if you're accessing the API from your browser and are prompted for a username and password. You can obtain an API key from https://platform.openai.com/account/api-keys.",
    ),
    template(
        "Codex backend 401 token invalidated",
        "Your authentication token has been invalidated. Please try signing in again.",
    ),
    template(
        "Codex backend 400/401 token invalidated (historical wording)",
        "Your authentication token has been invalidated. Please sign in again.",
    ),
    template(
        "Codex backend 401 token expired",
        "Provided authentication token is expired. Please try signing in again.",
    ),
    template(
        "Codex backend 401 refresh token reuse",
        "Your refresh token has already been used to generate a new access token. Please try signing in again.",
    ),
    template(
        "Gemini 400 API_KEY_INVALID",
        "API key not valid. Please pass a valid API key.",
    ),
    template(
        "Gemini 400 API key missing",
        "API Key not found. Please pass a valid API key.",
    ),
    template(
        "Gemini 403 unregistered caller",
        "Method doesn't allow unregistered callers (callers without established identity). Please use API Key or other form of API consumer identity to call this API.",
    ),
    template(
        "Gemini 401 UNAUTHENTICATED",
        "Request had invalid authentication credentials. Expected OAuth 2 access token, login cookie or other valid authentication credential. See https://developers.google.com/identity/sign-in/web/devconsole-project.",
    ),
    template(
        "DeepSeek 401",
        "Authentication Fails, Your api key: {api_key} is invalid",
    ),
    template("DeepSeek 401", "Authentication Fails (no such user)"),
    template("ACP auth_required (-32000)", "Authentication required"),
    // ---- Model not found / unsupported ------------------------------------------
    template(
        "OpenAI 404 model_not_found",
        "The model `{model}` does not exist or you do not have access to it.",
    ),
    template(
        "OpenAI 404 model_not_found",
        "The model `{model}` does not exist.",
    ),
    template(
        "OpenAI 400 model_not_found",
        "The requested model '{model}' does not exist.",
    ),
    template("Anthropic 404 not_found_error (model)", "model: {model}"),
    template(
        "Codex backend 400 ChatGPT-account model",
        "The '{model}' model is not supported when using Codex with a ChatGPT account.",
    ),
    template("Codex backend 400", "Unsupported model"),
    template("DeepSeek 400", "Model Not Exist"),
    template(
        "Gemini 404 NOT_FOUND model",
        "models/{model} is not found for API version {api_version}, or is not supported for {method}. Call ListModels to see the list of available models and their supported methods.",
    ),
    template(
        "Gemini 400 function calling",
        "Function calling is not enabled for {model}",
    ),
    template(
        "Gemini 400 system instruction",
        "Developer instruction is not enabled for {model}",
    ),
    // ---- Context window -------------------------------------------------------------
    template(
        "Anthropic 400 prompt too long",
        "prompt is too long: {int} tokens > {int} maximum",
    ),
    template(
        "Anthropic 400 context limit",
        "input length and `max_tokens` exceed context limit: {int} + {int} > {int}, decrease input length or `max_tokens` and try again",
    ),
    template(
        "Anthropic 400 max_tokens",
        "max_tokens: {int} > {int}, which is the maximum allowed number of output tokens for {model}",
    ),
    template(
        "OpenAI / DeepSeek 400 context_length_exceeded",
        "This model's maximum context length is {int} tokens. However, your messages resulted in {int} tokens.",
    ),
    template(
        "OpenAI / DeepSeek 400 context_length_exceeded",
        "This model's maximum context length is {int} tokens. However, your messages resulted in {int} tokens. Please reduce the length of the messages.",
    ),
    template(
        "OpenAI / DeepSeek 400 context_length_exceeded",
        "This model's maximum context length is {int} tokens. However, your messages resulted in {int} tokens (including {int} in the response_format schemas.). Please reduce the length of the messages or schemas.",
    ),
    template(
        "OpenAI / DeepSeek 400 context_length_exceeded",
        "This model's maximum context length is {int} tokens. However, you requested {int} tokens ({int} in the messages, {int} in the completion). Please reduce the length of the messages or completion.",
    ),
    template(
        "OpenAI 400 context_length_exceeded (short form)",
        "Please reduce the length of the messages or completion.",
    ),
    template(
        "OpenAI Responses 400 context_length_exceeded",
        "Your input exceeds the context window of this model. Please adjust your input and try again.",
    ),
    template(
        "Gemini 400 INVALID_ARGUMENT token count",
        "The input token count ({int}) exceeds the maximum number of tokens allowed ({int}).",
    ),
    template(
        "Gemini 400 INVALID_ARGUMENT token count",
        "The input token count exceeds the maximum number of tokens allowed ({int}).",
    ),
    // ---- Request shape / parameters ---------------------------------------------------
    template(
        "OpenAI 400 unsupported_parameter",
        "Unsupported parameter: '{param}' is not supported with this model.",
    ),
    template(
        "OpenAI 400 unsupported_parameter",
        "Unsupported parameter: '{param}' is not supported with this model. Use '{param}' instead.",
    ),
    template(
        "OpenAI 400 unsupported_value",
        "Unsupported value: '{param}' does not support {num} with this model. Only the default ({num}) value is supported.",
    ),
    template(
        "OpenAI 400 unknown_parameter",
        "Unknown parameter: '{param}'.",
    ),
    template(
        "OpenAI 400 unrecognized argument",
        "Unrecognized request argument supplied: {param}",
    ),
    template(
        "OpenAI 400 integer_below_min_value",
        "Invalid '{param}': integer below minimum value. Expected a value >= {int}, but got {int} instead.",
    ),
    template(
        "OpenAI 400 integer_above_max_value",
        "Invalid '{param}': integer above maximum value. Expected a value <= {int}, but got {int} instead.",
    ),
    template(
        "OpenAI 400 decimal_above_max_value",
        "Invalid '{param}': decimal above maximum value. Expected a value <= {num}, but got {num} instead.",
    ),
    template(
        "Anthropic 400 invalid_request_error (extra field)",
        "{param}: Extra inputs are not permitted",
    ),
    template(
        "Anthropic 400 invalid_request_error (field required)",
        "{param}: Field required",
    ),
    template(
        "Anthropic 400 invalid_request_error (sampling)",
        "`temperature` and `top_p` cannot both be specified for this model. Please use only one.",
    ),
    template(
        "Anthropic 400 invalid_request_error (thinking)",
        "`temperature` may only be set to 1 when thinking is enabled or in interleaved thinking mode.",
    ),
    template(
        "Anthropic 400 invalid_request_error (thinking budget)",
        "`max_tokens` must be greater than `thinking.budget_tokens`. Please consult our documentation at https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking#max-tokens-and-context-window-size",
    ),
    template(
        "Anthropic 400 invalid_request_error (orphaned tool_use)",
        "messages.{int}: `tool_use` ids were found without `tool_result` blocks immediately after: {tool_call_id}. Each `tool_use` block must have a corresponding `tool_result` block in the next message.",
    ),
    template(
        "Anthropic 400 invalid_request_error (orphaned tool_result)",
        "messages.{int}.content.{int}: unexpected `tool_use_id` found in `tool_result` blocks: {tool_call_id}. Each `tool_result` block must have a corresponding `tool_use` block in the previous message.",
    ),
    template(
        "Anthropic 400 invalid_request_error (empty text)",
        "messages: text content blocks must be non-empty",
    ),
    template(
        "Anthropic 413 request_too_large",
        "Request exceeds the maximum size",
    ),
    template("Codex backend 400", "Instructions are not valid"),
    template(
        "Gemini 400 INVALID_ARGUMENT",
        "Request contains an invalid argument.",
    ),
    template(
        "Gemini 400 INVALID_ARGUMENT unknown field",
        "Invalid JSON payload received. Unknown name \"{param}\" at '{param}': Cannot find field.",
    ),
    template(
        "Gemini 400 INVALID_ARGUMENT unknown field",
        "Invalid JSON payload received. Unknown name \"{param}\": Cannot find field.",
    ),
    template(
        "Gemini 400 INVALID_ARGUMENT thinking",
        "Thinking budget is not supported for this model.",
    ),
    template(
        "Gemini 400 INVALID_ARGUMENT multi-turn",
        "Please ensure that multiturn requests alternate between user and model.",
    ),
    template(
        "Gemini 400 INVALID_ARGUMENT function response",
        "Please ensure that function response turn comes immediately after a function call turn.",
    ),
    template(
        "Gemini 400 INVALID_ARGUMENT empty text",
        "Unable to submit request because it has an empty text parameter. Add a value to the parameter and try again.",
    ),
    template("ACP internal error (-32603)", "Internal error"),
    template("ACP method not found (-32601)", "Method not found"),
    template("ACP resource not found (-32002)", "Resource not found"),
    template("ACP invalid params (-32602)", "Invalid params"),
];

/// API parameter / field names that may appear in a `{param}` slot. A path
/// is accepted only when every dot-separated segment (after removing `[N]`
/// indices) is listed here. Sources: OpenAI Chat/Responses, Anthropic
/// Messages, Gemini generateContent request fields.
pub(crate) const PARAMETER_NAMES: &[&str] = &[
    // OpenAI / shared sampling and output.
    "model",
    "messages",
    "input",
    "instructions",
    "temperature",
    "top_p",
    "top_k",
    "n",
    "stop",
    "seed",
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
    "presence_penalty",
    "frequency_penalty",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "stream",
    "stream_options",
    "store",
    "metadata",
    "user",
    "service_tier",
    "parallel_tool_calls",
    "tool_choice",
    "tools",
    "functions",
    "function_call",
    "response_format",
    "reasoning",
    "reasoning_effort",
    "effort",
    "summary",
    "verbosity",
    "text",
    "format",
    "include",
    "truncation",
    "previous_response_id",
    "prompt_cache_key",
    "prompt_cache_retention",
    "safety_identifier",
    "modalities",
    "audio",
    "prediction",
    "web_search_options",
    "background",
    "conversation",
    "context_management",
    "include_obfuscation",
    // Anthropic Messages.
    "system",
    "thinking",
    "budget_tokens",
    "type",
    "stop_sequences",
    "anthropic_version",
    "anthropic_beta",
    "betas",
    "mcp_servers",
    "container",
    "cache_control",
    "content",
    "role",
    "name",
    "input_schema",
    "description",
    // Gemini generateContent.
    "contents",
    "parts",
    "generation_config",
    "generationConfig",
    "thinking_config",
    "thinkingConfig",
    "thinking_budget",
    "thinkingBudget",
    "thinking_level",
    "thinkingLevel",
    "include_thoughts",
    "includeThoughts",
    "system_instruction",
    "systemInstruction",
    "safety_settings",
    "safetySettings",
    "tool_config",
    "toolConfig",
    "function_declarations",
    "functionDeclarations",
    "function_calling_config",
    "functionCallingConfig",
    "response_mime_type",
    "responseMimeType",
    "response_schema",
    "responseSchema",
    "response_modalities",
    "responseModalities",
    "maxOutputTokens",
    "candidate_count",
    "candidateCount",
    "stopSequences",
    "topK",
    "topP",
    "media_resolution",
    "mediaResolution",
    "cached_content",
    "cachedContent",
    "labels",
    "parameters",
];

const RATE_UNITS: &[&str] = &[
    "tokens per min (TPM)",
    "requests per min (RPM)",
    "tokens per day (TPD)",
    "requests per day (RPD)",
    "input tokens per min (ITPM)",
    "output tokens per min (OTPM)",
    "images per min (IPM)",
];

const API_VERSIONS: &[&str] = &["v1", "v1beta", "v1alpha", "v1beta1"];

const GEMINI_METHODS: &[&str] = &[
    "generateContent",
    "streamGenerateContent",
    "countTokens",
    "embedContent",
    "batchEmbedContents",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Model,
    Int,
    Num,
    Param,
    Url,
    Duration,
    RateUnit,
    RequestId,
    ApiVersion,
    Method,
    AccountId,
    ApiKey,
    ToolCallId,
}

impl Slot {
    fn parse(name: &str) -> Self {
        match name {
            "model" => Self::Model,
            "int" => Self::Int,
            "num" => Self::Num,
            "param" => Self::Param,
            "url" => Self::Url,
            "duration" => Self::Duration,
            "rate_unit" => Self::RateUnit,
            "request_id" => Self::RequestId,
            "api_version" => Self::ApiVersion,
            "method" => Self::Method,
            "account_id" => Self::AccountId,
            "api_key" => Self::ApiKey,
            "tool_call_id" => Self::ToolCallId,
            other => panic!("unknown provider error template slot {{{other}}}"),
        }
    }

    /// Bounded shape the regex accepts; `validate` then decides by type.
    fn pattern(self) -> String {
        match self {
            Self::Model => r"[A-Za-z0-9][A-Za-z0-9._:@/-]{0,127}".to_owned(),
            Self::Int => r"(?:[0-9]{1,3}(?:,[0-9]{3}){1,3}|[0-9]{1,12})".to_owned(),
            Self::Num => r"[0-9]{1,9}(?:\.[0-9]{1,9})?".to_owned(),
            Self::Param => r"[A-Za-z0-9_.\[\]]{1,96}".to_owned(),
            Self::Url => r#"https?://[^\s<>"'()`]{1,256}?"#.to_owned(),
            Self::Duration => {
                r"[0-9]{1,6}(?:\.[0-9]{1,6})?(?:ms|s|m|h)(?:[0-9]{1,6}(?:\.[0-9]{1,6})?(?:ms|s|m))?"
                    .to_owned()
            }
            Self::RateUnit => alternation(RATE_UNITS),
            Self::RequestId => r"req[_-][A-Za-z0-9_-]{1,60}".to_owned(),
            Self::ApiVersion => alternation(API_VERSIONS),
            Self::Method => alternation(GEMINI_METHODS),
            Self::AccountId => r"[A-Za-z0-9_-]{1,96}".to_owned(),
            Self::ApiKey => r"[A-Za-z0-9*._-]{1,256}".to_owned(),
            Self::ToolCallId => r"(?:toolu_|call_|fc_)[A-Za-z0-9_-]{1,96}".to_owned(),
        }
    }

    /// Returns the published rendering of a slot value, or `None` when the
    /// value is not of the slot's type or not corroborated by `evidence`
    /// (the whole template then fails and the message is unknown).
    fn render(self, value: &str, evidence: &SlotEvidence<'_>) -> Option<String> {
        match self {
            Self::Model => model_corroborated(value, evidence).then(|| value.to_owned()),
            Self::Param => parameter_path(value).then(|| value.to_owned()),
            Self::Url => public_url_host(value),
            Self::RequestId => request_id_corroborated(value, evidence).then(|| value.to_owned()),
            Self::ToolCallId => evidence
                .tool_call_ids
                .iter()
                .any(|id| id == value)
                .then(|| value.to_owned()),
            Self::Int => {
                (value.bytes().filter(u8::is_ascii_digit).count() <= 12).then(|| value.to_owned())
            }
            Self::AccountId => account_id_shape(value).then(|| REDACTED.to_owned()),
            Self::ApiKey => Some(REDACTED.to_owned()),
            Self::Num | Self::Duration | Self::RateUnit | Self::ApiVersion | Self::Method => {
                Some(value.to_owned())
            }
        }
    }
}

/// What Haider itself sent or captured for the failed request. Slot values
/// that name a model, a tool call or a request id are published only when
/// they equal one of these; the default (no evidence) corroborates nothing
/// except a request id under the capped shape policy.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SlotEvidence<'a> {
    pub(crate) requested_model: Option<&'a str>,
    pub(crate) tool_call_ids: &'a [String],
    pub(crate) captured_request_id: Option<&'a str>,
}

const MAX_UNCAPTURED_REQUEST_ID_BYTES: usize = 64;

fn request_id_corroborated(value: &str, evidence: &SlotEvidence<'_>) -> bool {
    match evidence.captured_request_id {
        Some(captured) => captured == value,
        None => {
            value.len() <= MAX_UNCAPTURED_REQUEST_ID_BYTES
                && crate::error_detail::safe_request_id(value).is_some()
        }
    }
}

/// The requested model id (with or without a `models/` resource prefix), or
/// an id from a release-seeded offline catalog (Bedrock/Vertex).
fn model_corroborated(value: &str, evidence: &SlotEvidence<'_>) -> bool {
    let bare = |id: &str| id.strip_prefix("models/").unwrap_or(id).to_owned();
    let value_bare = bare(value);
    if evidence
        .requested_model
        .is_some_and(|requested| !requested.is_empty() && bare(requested) == value_bare)
    {
        return true;
    }
    crate::BEDROCK_SEED_MODELS
        .iter()
        .chain(crate::VERTEX_SEED_MODELS.iter())
        .any(|seeded| *seeded == value_bare)
}

fn alternation(options: &[&str]) -> String {
    format!(
        "(?:{})",
        options
            .iter()
            .map(|option| regex::escape(option))
            .collect::<Vec<_>>()
            .join("|")
    )
}

/// Array indices in a `{param}` path are at most this many digits.
const MAX_INDEX_DIGITS: usize = 4;

/// Every dot-separated segment is a known name (optionally followed by
/// `[N]` indices) or a bare array index (`messages.0.content`).
fn parameter_path(value: &str) -> bool {
    value.split('.').all(|segment| {
        if !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()) {
            return segment.len() <= MAX_INDEX_DIGITS;
        }
        let name = segment.find('[').map_or(segment, |index| &segment[..index]);
        let indices = &segment[name.len()..];
        PARAMETER_NAMES.contains(&name)
            && indices.split_inclusive(']').all(|index| {
                index
                    .strip_prefix('[')
                    .and_then(|index| index.strip_suffix(']'))
                    .is_some_and(|digits| {
                        (1..=MAX_INDEX_DIGITS).contains(&digits.len())
                            && digits.bytes().all(|byte| byte.is_ascii_digit())
                    })
            })
    })
}

/// Account ids are redacted regardless; the shape check only keeps a slot
/// from swallowing prose (`org-…`, `proj_…`, `user-…` or a UUID).
fn account_id_shape(value: &str) -> bool {
    ["org-", "org_", "proj_", "proj-", "user-", "user_", "acct_"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
        || (value.len() == 36
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'))
}

fn public_url_host(value: &str) -> Option<String> {
    let parsed = url::Url::parse(value).ok()?;
    let host = parsed.host_str()?;
    (matches!(parsed.scheme(), "http" | "https")
        && crate::error_detail::PUBLIC_PROVIDER_URL_HOSTS.contains(&host))
    .then(|| format!("{}://{host}", parsed.scheme()))
}

struct Compiled {
    regex: Regex,
    slots: Vec<Slot>,
}

fn compile(template: &Template) -> Compiled {
    let mut pattern = String::from("^");
    let mut slots = Vec::new();
    let mut rest = template.text;
    while let Some(open) = rest.find('{') {
        let close = rest[open..]
            .find('}')
            .map(|offset| open + offset)
            .expect("template slot is closed");
        pattern.push_str(&regex::escape(&rest[..open]));
        let slot = Slot::parse(&rest[open + 1..close]);
        pattern.push('(');
        pattern.push_str(&slot.pattern());
        pattern.push(')');
        slots.push(slot);
        rest = &rest[close + 1..];
    }
    pattern.push_str(&regex::escape(rest));
    pattern.push('$');
    Compiled {
        regex: Regex::new(&pattern).expect("static provider error template"),
        slots,
    }
}

static COMPILED: LazyLock<Vec<Compiled>> =
    LazyLock::new(|| TEMPLATES.iter().map(compile).collect());

/// Renders `message` from the first template it matches whole, with every
/// slot validated by type; `None` means the message is unknown.
pub(crate) fn render_known_provider_message(message: &str) -> Option<String> {
    render_known_provider_message_with(message, &SlotEvidence::default())
}

/// [`render_known_provider_message`] with corroborating request evidence.
pub(crate) fn render_known_provider_message_with(
    message: &str,
    evidence: &SlotEvidence<'_>,
) -> Option<String> {
    let message = message.trim();
    if message.is_empty() || message.len() > 2048 {
        return None;
    }
    COMPILED.iter().find_map(|compiled| {
        let captures = compiled.regex.captures(message)?;
        let mut rendered = String::with_capacity(message.len());
        let mut cursor = 0;
        for (index, slot) in compiled.slots.iter().enumerate() {
            let value = captures.get(index + 1)?;
            rendered.push_str(&message[cursor..value.start()]);
            rendered.push_str(&slot.render(value.as_str(), evidence)?);
            cursor = value.end();
        }
        rendered.push_str(&message[cursor..]);
        Some(rendered)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_compiles_and_matches_its_own_literal_form() {
        assert_eq!(COMPILED.len(), TEMPLATES.len());
        for template in TEMPLATES {
            if template.text.contains('{') {
                continue;
            }
            assert_eq!(
                render_known_provider_message(template.text).as_deref(),
                Some(template.text),
                "{}",
                template.provenance
            );
        }
    }

    #[test]
    fn slot_types_are_enforced() {
        let requested = SlotEvidence {
            requested_model: Some("gpt-4o"),
            ..SlotEvidence::default()
        };
        assert!(model_corroborated("gpt-4o", &requested));
        assert!(!model_corroborated("gpt-4o-quillmere", &requested));
        assert!(!model_corroborated("gpt-4o", &SlotEvidence::default()));
        let gemini = SlotEvidence {
            requested_model: Some("gemini-2.5-pro"),
            ..SlotEvidence::default()
        };
        assert!(model_corroborated("models/gemini-2.5-pro", &gemini));
        let ids = vec!["toolu_fixture01".to_owned()];
        let tools = SlotEvidence {
            tool_call_ids: &ids,
            ..SlotEvidence::default()
        };
        assert_eq!(
            Slot::ToolCallId
                .render("toolu_fixture01", &tools)
                .as_deref(),
            Some("toolu_fixture01")
        );
        assert_eq!(Slot::ToolCallId.render("toolu_other", &tools), None);
        let captured = SlotEvidence {
            captured_request_id: Some("req_captured01"),
            ..SlotEvidence::default()
        };
        assert!(request_id_corroborated("req_captured01", &captured));
        assert!(!request_id_corroborated("req_other01", &captured));
        assert!(request_id_corroborated(
            "req_other01",
            &SlotEvidence::default()
        ));
        assert!(!request_id_corroborated(
            &format!("req_{}", "a".repeat(61)),
            &SlotEvidence::default()
        ));
        assert!(parameter_path("messages.9999.content"));
        assert!(!parameter_path("messages.10000.content"));
        assert!(!parameter_path("tools[12345]"));
        assert_eq!(
            render_known_provider_message("prompt is too long: 40,000 tokens > 1,000,000 maximum")
                .as_deref(),
            Some("prompt is too long: 40,000 tokens > 1,000,000 maximum")
        );
        assert_eq!(
            render_known_provider_message("prompt is too long: 4155550123999 tokens > 1 maximum"),
            None
        );
        assert!(parameter_path("temperature"));
        assert!(parameter_path("generation_config.thinking_config"));
        assert!(parameter_path(
            "tools[0].function_declarations[12].parameters"
        ));
        assert!(!parameter_path("quillmere"));
        assert!(!parameter_path("tools[x]"));
        assert!(!parameter_path("tools[]"));
        assert!(parameter_path("messages.0.content.1.cache_control"));
        assert!(!parameter_path("messages..content"));
        assert_eq!(
            public_url_host("https://platform.openai.com/docs/x?y=1").as_deref(),
            Some("https://platform.openai.com")
        );
        assert_eq!(public_url_host("https://quillmere.openai.com/"), None);
        assert!(account_id_shape("org-fixture123"));
        assert!(!account_id_shape("quillmere"));
    }
}
