//! Adapter for servers that speak the OpenAI Chat Completions shape: OpenAI
//! itself and the many local and hosted servers that copy its
//! `/chat/completions` endpoint.
//!
//! Translation only, as with every adapter. `params` is passed through as
//! the request body's other fields (`temperature`, `max_completion_tokens`,
//! `tool_choice` and so on), so the agent decides them. The adapter adds
//! nothing but `stream` and, when streaming, the request for usage in the
//! last chunk.
//!
//! What this shape cannot carry:
//!
//! - A tool result has no error flag. `is_error` is not sent; the content
//!   goes as it is.
//! - Reasoning text some servers return beside the answer (`reasoning` or
//!   `reasoning_content`) is not put in the message, because servers disagree
//!   on whether it may be sent back. It is still in the raw body the trace
//!   keeps.
//! - Only the first choice is read. Do not ask for `n` above 1.
//!
//! A refusal comes back as a [`Block::Provider`] and goes back out in the
//! message's `refusal` field.
//!
//! Prices: the built-in table has no rows for these models, so their calls
//! carry no price unless a table is given, with [`OpenAiCompatAdapter::pricing`]
//! or the `TETHER_PRICING` file.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tether_types::{Price, Usage};

use crate::http::{self, env, provider, Failed, Sse};
use crate::{
    Block, Completion, CompletionRequest, Message, ModelAdapter, ModelError, Pricing, Role,
};

/// Name on [`Block::Provider`] blocks this adapter makes and accepts back,
/// and the provider recorded in the trace.
pub const PROVIDER: &str = "openai_compat";

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiCompatAdapter {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    stream: bool,
    pricing: Pricing,
    max_retries: u32,
    retry_delay: Duration,
}

impl OpenAiCompatAdapter {
    /// An adapter for the server at `base_url`, the address that
    /// `/chat/completions` is appended to: `https://api.openai.com/v1`, or
    /// `http://localhost:11434/v1` for a local server. It sends no
    /// credentials until [`api_key`](Self::api_key) gives some.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: http::client(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: None,
            stream: true,
            pricing: Pricing::builtin(),
            max_retries: 2,
            retry_delay: Duration::from_millis(500),
        }
    }

    /// From the environment: `OPENAI_BASE_URL` for the server, defaulting to
    /// OpenAI's, and `OPENAI_API_KEY`. The key may be left out for a server
    /// that needs none, but not for the default one. `TETHER_PRICING` names a
    /// pricing table to use in place of the built-in one.
    pub fn from_env() -> Result<Self, ModelError> {
        let base_url = env("OPENAI_BASE_URL");
        let key = env("OPENAI_API_KEY");
        if base_url.is_none() && key.is_none() {
            return Err(ModelError::Credentials(
                "set OPENAI_API_KEY, or OPENAI_BASE_URL for a server that needs no key".into(),
            ));
        }
        let mut adapter = Self::new(base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into()))
            .pricing(Pricing::from_env()?);
        adapter.api_key = key;
        Ok(adapter)
    }

    /// Send `key` as a bearer token on every request.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
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

    /// How many times to retry a rate limit or a server error, and how long
    /// to wait before the first retry. The wait doubles each time unless the
    /// response says how long to wait.
    pub fn retries(mut self, max_retries: u32, first_delay: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_delay = first_delay;
        self
    }

    fn request(&self, body: Vec<u8>) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .header("content-type", "application/json");
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        request.body(body)
    }

    async fn attempt(&self, body: &[u8], started: Instant) -> Result<Completion, Failed> {
        let (mut response, request_id) =
            http::send(self.request(body.to_vec()), "x-request-id", api_error).await?;

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

        let mut completion = parse_response(&message).map_err(Failed::fatal)?;
        completion.ttft_ms = ttft_ms;
        completion.stream = self.stream;
        completion.provider_request_id = request_id;
        completion.raw = Some(raw);
        Ok(completion)
    }
}

/// An error status as a [`ModelError::Api`], from the server's error body.
fn api_error(status: u16, body: &str, request_id: Option<String>) -> ModelError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = &parsed["error"];
    let field = |name: &str| error[name].as_str().map(str::to_owned);
    ModelError::Api {
        status,
        kind: field("type")
            .or_else(|| field("code"))
            .unwrap_or_else(|| "error".into()),
        // Some servers send the error as a bare string.
        message: field("message")
            .or_else(|| error.as_str().map(str::to_owned))
            .unwrap_or_else(|| body.trim().to_owned()),
        request_id,
    }
}

#[async_trait]
impl ModelAdapter for OpenAiCompatAdapter {
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
    if stream {
        body.insert("stream".into(), json!(true));
        // Without this a streamed response reports no token counts.
        body.entry("stream_options")
            .or_insert(json!({"include_usage": true}));
    }

    let messages: Vec<Value> = req.messages.iter().flat_map(messages_to_api).collect();
    body.insert("messages".into(), json!(messages));

    if !req.tools.is_empty() {
        let defined = req.tools.iter().map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                },
            })
        });
        // Tools the agent put in params come first.
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

/// One of Tether's messages as the provider's messages. Each tool result is
/// a message of its own here, so one message can become several; one left
/// empty by blocks from another provider becomes none.
fn messages_to_api(message: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    let mut texts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut refusal = None;
    for block in &message.content {
        match block {
            Block::Text { text } => texts.push(text.as_str()),
            Block::ToolUse {
                id,
                name,
                arguments,
            } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                // Arguments travel as JSON text in this shape.
                "function": {"name": name, "arguments": arguments.to_string()},
            })),
            Block::ToolResult { id, content, .. } => {
                let text = match content {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                out.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
            }
            Block::Provider { provider, block } if provider == PROVIDER => {
                if let Some(text) = block["refusal"].as_str() {
                    refusal = Some(text);
                }
            }
            Block::Provider { .. } => {}
        }
    }

    // One text goes as a plain string, which every server takes; several go
    // as parts so that none is altered by joining.
    let content = match texts.as_slice() {
        [] => Value::Null,
        [text] => json!(text),
        parts => parts
            .iter()
            .map(|text| json!({"type": "text", "text": text}))
            .collect(),
    };
    let role = match message.role {
        Role::System => "system",
        Role::Assistant => "assistant",
        // Text beside tool results is the user speaking.
        Role::User | Role::Tool => "user",
    };
    if message.role == Role::Assistant {
        if !content.is_null() || !tool_calls.is_empty() || refusal.is_some() {
            let mut turn = json!({"role": role, "content": content});
            if !tool_calls.is_empty() {
                turn["tool_calls"] = json!(tool_calls);
            }
            if let Some(refusal) = refusal {
                turn["refusal"] = json!(refusal);
            }
            out.push(turn);
        }
    } else if !content.is_null() {
        // Tool results answer the turn before, so they stay ahead of the text.
        out.push(json!({"role": role, "content": content}));
    }
    out
}

/// A whole response body as a [`Completion`]. The caller fills in what only
/// it knows: timing, the request id and the raw body.
pub fn parse_response(body: &Value) -> Result<Completion, ModelError> {
    if !body["error"].is_null() {
        return Err(api_error(200, &body.to_string(), None));
    }
    let choice = &body["choices"][0];
    let message = choice["message"]
        .as_object()
        .ok_or_else(|| provider("response has no message in its first choice"))?;

    let mut content = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => {
            content.push(Block::Text { text: text.clone() });
        }
        // Some servers answer in parts.
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(text) = part["text"].as_str() {
                    content.push(Block::Text { text: text.into() });
                }
            }
        }
        _ => {}
    }
    if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
        content.push(Block::Provider {
            provider: PROVIDER.into(),
            block: json!({"type": "refusal", "refusal": refusal}),
        });
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = &call["function"];
        let (Some(id), Some(name)) = (call["id"].as_str(), function["name"].as_str()) else {
            return Err(provider(format!(
                "tool call without an id or a name: {call}"
            )));
        };
        let arguments = match function["arguments"].as_str() {
            // A tool with no parameters is sometimes called with no text at all.
            None | Some("") => json!({}),
            Some(text) => serde_json::from_str(text).map_err(|e| {
                provider(format!(
                    "arguments for {name} are not complete JSON, the response was probably cut off: {e}"
                ))
            })?,
        };
        content.push(Block::ToolUse {
            id: id.into(),
            name: name.into(),
            arguments,
        });
    }

    let usage = &body["usage"];
    let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
    // Tokens read from the cache are counted inside `prompt_tokens` here.
    // Tether counts them apart, since they are priced apart.
    let cached = usage["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0)
        .min(prompt);
    Ok(Completion {
        message: Message {
            role: Role::Assistant,
            content,
        },
        model: body["model"].as_str().map(str::to_owned),
        stop_reason: choice["finish_reason"].as_str().map(str::to_owned),
        usage: Usage {
            input_tokens: prompt - cached,
            output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
            cache_read_tokens: cached,
            cache_write_tokens: 0,
        },
        ttft_ms: None,
        stream: false,
        provider_request_id: None,
        raw: None,
    })
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Collapses a stream of chat completion chunks into the body a
/// non-streaming request would have returned. Only the first choice is kept.
#[derive(Default)]
pub struct StreamCollector {
    sse: Sse,
    /// Top-level fields of the chunks other than `choices`: `id`, `model`,
    /// and `usage` once the last chunk brings it.
    head: Map<String, Value>,
    content: String,
    refusal: Option<String>,
    calls: Vec<PartialCall>,
    finish_reason: Option<Value>,
    saw_content: bool,
    done: bool,
}

impl StreamCollector {
    /// True once any content has arrived, for time to first token.
    pub fn saw_content(&self) -> bool {
        self.saw_content
    }

    /// Take the next bytes of the stream. Chunks may split anywhere.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), ModelError> {
        for data in self.sse.feed(chunk) {
            if data.trim() == "[DONE]" {
                self.done = true;
            } else {
                self.chunk(&data)?;
            }
        }
        Ok(())
    }

    fn chunk(&mut self, data: &str) -> Result<(), ModelError> {
        let chunk: Value = serde_json::from_str(data)
            .map_err(|e| provider(format!("stream chunk is not JSON: {e}")))?;
        if !chunk["error"].is_null() {
            return Err(api_error(200, data, None));
        }
        let Value::Object(fields) = chunk else {
            return Err(provider("stream chunk is not an object"));
        };
        let mut choices = Value::Null;
        for (name, value) in fields {
            if name == "choices" {
                choices = value;
            } else if !value.is_null() {
                // Later chunks repeat `id` and `model`; `usage` comes last.
                self.head.insert(name, value);
            }
        }
        let Some(choice) = choices
            .as_array()
            .and_then(|all| all.iter().find(|c| c["index"].as_u64().unwrap_or(0) == 0))
        else {
            return Ok(());
        };

        let delta = &choice["delta"];
        let piece = |name: &str| delta[name].as_str().filter(|text| !text.is_empty());
        if let Some(text) = piece("content") {
            self.content.push_str(text);
            self.saw_content = true;
        }
        if let Some(text) = piece("refusal") {
            self.refusal.get_or_insert_default().push_str(text);
            self.saw_content = true;
        }
        if piece("reasoning_content").or(piece("reasoning")).is_some() {
            self.saw_content = true;
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            self.saw_content = true;
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            if self.calls.len() <= index {
                self.calls.resize_with(index + 1, PartialCall::default);
            }
            let partial = &mut self.calls[index];
            let function = &call["function"];
            if let Some(id) = call["id"].as_str().filter(|id| !id.is_empty()) {
                partial.id = id.into();
            }
            // The name comes whole in the first piece; arguments come in pieces.
            if let Some(name) = function["name"].as_str().filter(|name| !name.is_empty()) {
                partial.name = name.into();
            }
            if let Some(text) = function["arguments"].as_str() {
                partial.arguments.push_str(text);
            }
        }
        if !choice["finish_reason"].is_null() {
            self.finish_reason = Some(choice["finish_reason"].clone());
        }
        Ok(())
    }

    /// The finished body. An error if the stream ended early.
    pub fn finish(mut self) -> Result<Value, ModelError> {
        if !self.done && self.finish_reason.is_none() {
            return Err(provider("stream ended before the message was complete"));
        }
        let mut message = json!({
            "role": "assistant",
            "content": if self.content.is_empty() { Value::Null } else { json!(self.content) },
        });
        if let Some(refusal) = self.refusal {
            message["refusal"] = json!(refusal);
        }
        if !self.calls.is_empty() {
            message["tool_calls"] = self
                .calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": call.arguments},
                    })
                })
                .collect();
        }
        if self.head.get("object").and_then(Value::as_str) == Some("chat.completion.chunk") {
            self.head.insert("object".into(), json!("chat.completion"));
        }
        self.head.insert(
            "choices".into(),
            json!([{
                "index": 0,
                "message": message,
                "finish_reason": self.finish_reason.unwrap_or(Value::Null),
            }]),
        );
        Ok(Value::Object(self.head))
    }
}
