//! Adapter for the Anthropic Messages API.
//!
//! Translation only. The request is the agent's messages, tools and params
//! put into the provider's shape; nothing is added to a prompt. `params` is
//! passed through as the request body's other fields (`max_tokens`,
//! `thinking`, `output_config`, `fallbacks` and so on), so the agent decides
//! them, not this adapter.
//!
//! Blocks the adapter does not model (reasoning blocks and their signatures,
//! server tool results, fallback markers) come back as [`Block::Provider`]
//! holding the provider's JSON untouched, and go back out the same way. The
//! API rejects a conversation whose earlier reasoning blocks were altered or
//! dropped from the middle, so an agent should append each reply to its
//! history as it received it.
//!
//! Responses are streamed by default and collapsed into one [`Completion`]
//! with `stream: true` and `ttft_ms`. The raw body, the whole event stream
//! when streaming, is returned for the trace.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tether_types::{Price, Usage};

use crate::http::{self, env, provider, Failed, Sse};
use crate::{
    Block, Completion, CompletionRequest, Message, ModelAdapter, ModelError, Pricing, Role,
};

/// Name on [`Block::Provider`] blocks this adapter makes and accepts back.
pub const PROVIDER: &str = "anthropic";

/// The API version every request is sent with. One per release.
pub const API_VERSION: &str = "2026-10-21";

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// `max_tokens` when the agent's params give none. Streaming has no request
/// timeout to stay under, so it gets more room.
const DEFAULT_MAX_TOKENS_STREAMING: u64 = 64_000;
const DEFAULT_MAX_TOKENS: u64 = 16_000;

/// Needed to send an OAuth bearer token instead of an API key.
const OAUTH_BETA: &str = "oauth-2025-04-20";

enum Auth {
    ApiKey(String),
    Bearer(String),
}

pub struct AnthropicAdapter {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
    betas: Vec<String>,
    stream: bool,
    pricing: Pricing,
    max_retries: u32,
    retry_delay: Duration,
}

impl AnthropicAdapter {
    /// An adapter that authenticates with an API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_auth(Auth::ApiKey(api_key.into()))
    }

    /// Credentials from the environment: `ANTHROPIC_API_KEY`, or failing that
    /// an OAuth token in `ANTHROPIC_AUTH_TOKEN`. `ANTHROPIC_BASE_URL`
    /// overrides the endpoint, and `TETHER_PRICING` names a pricing table to
    /// use in place of the built-in one.
    pub fn from_env() -> Result<Self, ModelError> {
        let mut adapter = if let Some(key) = env("ANTHROPIC_API_KEY") {
            Self::new(key)
        } else if let Some(token) = env("ANTHROPIC_AUTH_TOKEN") {
            Self::with_auth(Auth::Bearer(token)).beta(OAUTH_BETA)
        } else {
            return Err(ModelError::Credentials(
                "set ANTHROPIC_API_KEY (or ANTHROPIC_AUTH_TOKEN)".into(),
            ));
        };
        if let Some(url) = env("ANTHROPIC_BASE_URL") {
            adapter = adapter.base_url(url);
        }
        Ok(adapter.pricing(Pricing::from_env()?))
    }

    fn with_auth(auth: Auth) -> Self {
        Self {
            http: http::client(),
            base_url: DEFAULT_BASE_URL.into(),
            auth,
            betas: Vec::new(),
            stream: true,
            pricing: Pricing::builtin(),
            max_retries: 2,
            retry_delay: Duration::from_millis(500),
        }
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Send a value in the `anthropic-beta` header on every request. A beta
    /// field in the agent's params needs its header here.
    pub fn beta(mut self, beta: impl Into<String>) -> Self {
        self.betas.push(beta.into());
        self
    }

    /// Stream responses (the default) or ask for one JSON body.
    pub fn streaming(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    pub fn pricing(mut self, pricing: Pricing) -> Self {
        self.pricing = pricing;
        self
    }

    /// How many times to retry a rate limit, an overload or a server error,
    /// and how long to wait before the first retry. The wait doubles each
    /// time unless the response says how long to wait.
    pub fn retries(mut self, max_retries: u32, first_delay: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_delay = first_delay;
        self
    }

    fn request(&self, body: Vec<u8>) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("content-type", "application/json")
            .header("anthropic-version", API_VERSION);
        request = match &self.auth {
            Auth::ApiKey(key) => request.header("x-api-key", key),
            Auth::Bearer(token) => request.bearer_auth(token),
        };
        if !self.betas.is_empty() {
            request = request.header("anthropic-beta", self.betas.join(","));
        }
        request.body(body)
    }

    /// One HTTP attempt. `Err(Failed { retry: true, .. })` is worth trying again.
    async fn attempt(&self, body: &[u8], started: Instant) -> Result<Completion, Failed> {
        let (mut response, request_id) =
            http::send(self.request(body.to_vec()), "request-id", api_error).await?;

        // Past this point output may already be billed, so nothing retries.
        let fatal = |e: reqwest::Error| Failed::fatal(provider(format!("reading response: {e}")));
        let mut raw = Vec::new();
        let mut ttft_ms = None;
        let message = if self.stream {
            let mut collector = StreamCollector::default();
            while let Some(chunk) = response.chunk().await.map_err(fatal)? {
                raw.extend_from_slice(&chunk);
                collector.feed(&chunk).map_err(Failed::fatal)?;
                if ttft_ms.is_none() && collector.saw_content() {
                    ttft_ms = Some(started.elapsed().as_millis() as u64);
                }
            }
            collector.finish().map_err(Failed::fatal)?
        } else {
            while let Some(chunk) = response.chunk().await.map_err(fatal)? {
                raw.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&raw)
                .map_err(|e| Failed::fatal(provider(format!("response is not JSON: {e}"))))?
        };

        let mut completion = parse_message(&message).map_err(Failed::fatal)?;
        completion.ttft_ms = ttft_ms;
        completion.stream = self.stream;
        completion.provider_request_id = request_id;
        completion.raw = Some(raw);
        Ok(completion)
    }
}

/// An error status as a [`ModelError::Api`], from the provider's error body.
fn api_error(status: u16, body: &str, request_id: Option<String>) -> ModelError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let field = |name: &str| parsed["error"][name].as_str().map(str::to_owned);
    ModelError::Api {
        status,
        kind: field("type").unwrap_or_else(|| "error".into()),
        message: field("message").unwrap_or_else(|| body.trim().to_owned()),
        request_id: parsed["request_id"]
            .as_str()
            .map(str::to_owned)
            .or(request_id),
    }
}

#[async_trait]
impl ModelAdapter for AnthropicAdapter {
    fn id(&self) -> &str {
        PROVIDER
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ModelError> {
        let body = serde_json::to_vec(&request_body(&req, self.stream)?)
            .map_err(|e| provider(format!("encoding request: {e}")))?;
        let started = Instant::now();
        http::with_retries(self.max_retries, self.retry_delay, || {
            self.attempt(&body, started)
        })
        .await
    }

    fn price(&self, model: &str, usage: &Usage) -> Option<Price> {
        self.pricing.price(model, usage)
    }
}

/// The request body for `req`, exactly as it would be sent.
pub fn request_body(req: &CompletionRequest, stream: bool) -> Result<Value, ModelError> {
    let mut body = match &req.params {
        Value::Object(params) => params.clone(),
        Value::Null => Map::new(),
        other => return Err(provider(format!("params must be an object, got {other}"))),
    };
    body.insert("model".into(), json!(req.model));
    body.entry("max_tokens").or_insert(json!(if stream {
        DEFAULT_MAX_TOKENS_STREAMING
    } else {
        DEFAULT_MAX_TOKENS
    }));
    if stream {
        body.insert("stream".into(), json!(true));
    }

    // System messages at the head of the conversation are the system prompt.
    // Later ones stay in the conversation as mid-conversation instructions.
    let lead = req
        .messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    if lead > 0 {
        if body.contains_key("system") {
            return Err(provider(
                "the system prompt is given twice: in params and as a leading system message",
            ));
        }
        let system: Vec<Value> = req.messages[..lead]
            .iter()
            .flat_map(|m| m.content.iter().filter_map(block_to_api))
            .collect();
        body.insert("system".into(), json!(system));
    }

    let messages: Vec<Value> = req.messages[lead..]
        .iter()
        .filter_map(|message| {
            let content: Vec<Value> = message.content.iter().filter_map(block_to_api).collect();
            let role = match message.role {
                Role::User | Role::Tool => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };
            // A message left empty by blocks from another provider is dropped.
            (!content.is_empty()).then(|| json!({"role": role, "content": content}))
        })
        .collect();
    body.insert("messages".into(), json!(messages));

    if !req.tools.is_empty() {
        let defined = req.tools.iter().map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        });
        // Tools the agent put in params (server tools, say) come first.
        match body.entry("tools").or_insert(json!([])) {
            Value::Array(tools) => tools.extend(defined),
            other => {
                return Err(provider(format!(
                    "params.tools must be a list, got {other}"
                )))
            }
        }
    }
    Ok(Value::Object(body))
}

/// A block in the provider's shape, or `None` for another provider's block.
fn block_to_api(block: &Block) -> Option<Value> {
    Some(match block {
        Block::Text { text } => json!({"type": "text", "text": text}),
        Block::ToolUse {
            id,
            name,
            arguments,
        } => json!({"type": "tool_use", "id": id, "name": name, "input": arguments}),
        Block::ToolResult {
            id,
            content,
            is_error,
        } => {
            // The API takes text here; anything else goes as its JSON text.
            let text = match content {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let mut result = json!({"type": "tool_result", "tool_use_id": id, "content": text});
            if *is_error {
                result["is_error"] = json!(true);
            }
            result
        }
        Block::Provider { provider, block } if provider == PROVIDER => block.clone(),
        Block::Provider { .. } => return None,
    })
}

/// A block from the provider. Plain text and tool calls become Tether's own
/// blocks; everything else is kept whole.
fn block_from_api(block: &Value) -> Block {
    let kept = || Block::Provider {
        provider: PROVIDER.into(),
        block: block.clone(),
    };
    let Some(fields) = block.as_object() else {
        return kept();
    };
    let text = |name: &str| fields.get(name).and_then(Value::as_str).map(str::to_owned);
    match fields.get("type").and_then(Value::as_str) {
        // Text carrying citations has more to it than `Block::Text` holds.
        Some("text") if fields.get("citations").is_none_or(Value::is_null) => match text("text") {
            Some(text) => Block::Text { text },
            None => kept(),
        },
        Some("tool_use") => match (text("id"), text("name"), fields.get("input")) {
            (Some(id), Some(name), Some(input)) => Block::ToolUse {
                id,
                name,
                arguments: input.clone(),
            },
            _ => kept(),
        },
        _ => kept(),
    }
}

/// A whole response message as a [`Completion`]. The caller fills in what
/// only it knows: timing, the request id and the raw body.
pub fn parse_message(message: &Value) -> Result<Completion, ModelError> {
    if message["type"] == "error" {
        return Err(api_error(200, &message.to_string(), None));
    }
    let content = message["content"]
        .as_array()
        .ok_or_else(|| provider("response has no content list"))?;
    let tokens = |name: &str| message["usage"][name].as_u64().unwrap_or(0);
    let text = |name: &str| message[name].as_str().map(str::to_owned);
    Ok(Completion {
        message: Message {
            role: Role::Assistant,
            content: content.iter().map(block_from_api).collect(),
        },
        model: text("model"),
        stop_reason: text("stop_reason"),
        usage: Usage {
            input_tokens: tokens("input_tokens"),
            output_tokens: tokens("output_tokens"),
            cache_read_tokens: tokens("cache_read_input_tokens"),
            cache_write_tokens: tokens("cache_creation_input_tokens"),
        },
        ttft_ms: None,
        stream: false,
        provider_request_id: None,
        raw: None,
    })
}

/// Collapses a server-sent event stream into the message it describes, the
/// same JSON a non-streaming request would have returned.
///
/// An event type it does not know is skipped, as the API asks of clients. A
/// content delta it does not know is an error: guessing would corrupt a block
/// that has to go back to the provider unchanged. Ask for a non-streaming
/// response if that ever happens.
#[derive(Default)]
pub struct StreamCollector {
    sse: Sse,
    message: Option<Map<String, Value>>,
    blocks: Vec<Value>,
    /// Tool input arrives as pieces of JSON text, one string per block.
    partial_input: Vec<String>,
    saw_content: bool,
    stopped: bool,
}

impl StreamCollector {
    /// True once any content has arrived, for time to first token.
    pub fn saw_content(&self) -> bool {
        self.saw_content
    }

    /// Take the next bytes of the stream. Chunks may split anywhere.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), ModelError> {
        for data in self.sse.feed(chunk) {
            self.event(&data)?;
        }
        Ok(())
    }

    fn block(&mut self, index: usize) -> Result<&mut Value, ModelError> {
        self.blocks.get_mut(index).ok_or_else(|| {
            provider(format!(
                "stream names content block {index} before starting it"
            ))
        })
    }

    fn event(&mut self, data: &str) -> Result<(), ModelError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|e| provider(format!("stream event is not JSON: {e}")))?;
        let index = event["index"].as_u64().unwrap_or(0) as usize;
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                self.message = event["message"].as_object().cloned();
            }
            "content_block_start" => {
                if self.blocks.len() <= index {
                    self.blocks.resize(index + 1, Value::Null);
                    self.partial_input.resize(index + 1, String::new());
                }
                self.blocks[index] = event["content_block"].clone();
                self.saw_content = true;
            }
            "content_block_delta" => {
                self.saw_content = true;
                let delta = &event["delta"];
                let piece = |name: &str| delta[name].as_str().unwrap_or_default().to_owned();
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => append(self.block(index)?, "text", &piece("text")),
                    "thinking_delta" => append(self.block(index)?, "thinking", &piece("thinking")),
                    "signature_delta" => {
                        self.block(index)?["signature"] = json!(piece("signature"));
                    }
                    "citations_delta" => {
                        let block = self.block(index)?;
                        match &mut block["citations"] {
                            Value::Array(citations) => citations.push(delta["citation"].clone()),
                            slot => *slot = json!([delta["citation"]]),
                        }
                    }
                    "input_json_delta" => {
                        self.block(index)?;
                        self.partial_input[index].push_str(&piece("partial_json"));
                    }
                    other => {
                        return Err(provider(format!(
                            "stream sent a content delta this build does not know: {other:?}"
                        )));
                    }
                }
            }
            "content_block_stop" => {
                let input =
                    std::mem::take(self.partial_input.get_mut(index).ok_or_else(|| {
                        provider("stream stops a content block it never started")
                    })?);
                if !input.is_empty() {
                    let parsed = serde_json::from_str::<Value>(&input).map_err(|e| {
                        provider(format!(
                            "tool input is not complete JSON, the response was probably cut off: {e}"
                        ))
                    })?;
                    self.block(index)?["input"] = parsed;
                }
            }
            "message_delta" => {
                let message = self
                    .message
                    .as_mut()
                    .ok_or_else(|| provider("stream sent message_delta before message_start"))?;
                // `stop_reason` and whatever else the message ends with.
                if let Some(delta) = event["delta"].as_object() {
                    message.extend(delta.clone());
                }
                // Usage here is the running total and replaces what came before.
                if let Some(usage) = event["usage"].as_object() {
                    match message.entry("usage").or_insert(json!({})) {
                        Value::Object(total) => total.extend(usage.clone()),
                        slot => *slot = Value::Object(usage.clone()),
                    }
                }
            }
            "message_stop" => self.stopped = true,
            "error" => return Err(api_error(200, data, None)),
            // `ping`, and event types added after this was written.
            _ => {}
        }
        Ok(())
    }

    /// The finished message. An error if the stream ended early.
    pub fn finish(self) -> Result<Value, ModelError> {
        let mut message = self
            .message
            .ok_or_else(|| provider("stream ended before the message started"))?;
        if !self.stopped {
            return Err(provider("stream ended before the message was complete"));
        }
        message.insert("content".into(), Value::Array(self.blocks));
        Ok(Value::Object(message))
    }
}

/// Append `piece` to the string field `name` of `block`.
fn append(block: &mut Value, name: &str, piece: &str) {
    match &mut block[name] {
        Value::String(text) => text.push_str(piece),
        slot => *slot = json!(piece),
    }
}
