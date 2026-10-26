//! Anthropic Messages API <-> OpenAI Chat Completions translation.
//!
//! OpenCode Zen exposes an OpenAI-compatible surface; Claude Code speaks the
//! Anthropic Messages API. Every function here is pure `serde_json` in and
//! `String`/`Value` out, so the whole protocol bridge is unit-testable
//! without touching the network.
//!
//! Three things are translated:
//!   * the request body (`anthropic_to_opencode_request`)
//!   * a non-streamed response (`opencode_response_to_anthropic`)
//!   * a streamed response ([`SseTranslator`])

use serde_json::{json, Map, Value};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Tool id round-tripping
// ---------------------------------------------------------------------------

/// Anthropic tool_use ids are expected to carry a `toolu_` prefix; OpenCode
/// Zen hands back OpenAI-style `call_…` ids. We prefix on the way out and strip
/// the same prefix on the way back so the correlation is lossless.
fn to_anthropic_tool_id(id: &str) -> String {
    if id.starts_with("toolu_") {
        id.to_string()
    } else {
        format!("toolu_{}", id)
    }
}

fn to_openai_tool_id(id: &str) -> String {
    id.strip_prefix("toolu_").unwrap_or(id).to_string()
}

// ---------------------------------------------------------------------------
// Request: Anthropic -> OpenAI
// ---------------------------------------------------------------------------

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// Flatten Anthropic's `system` field (string or array of text blocks).
fn system_text(body: &Value) -> Option<String> {
    match body.get("system")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let joined: Vec<&str> = blocks.iter().filter_map(|b| str_of(b, "text")).collect();
            if joined.is_empty() {
                None
            } else {
                Some(joined.join("\n"))
            }
        }
        _ => None,
    }
}

/// Convert a single Anthropic image block into an OpenAI `image_url` part.
fn anthropic_image_to_openai(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    match str_of(source, "type") {
        Some("base64") => {
            let media = str_of(source, "media_type").unwrap_or("image/png");
            let data = str_of(source, "data")?;
            Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", media, data) }
            }))
        }
        Some("url") => {
            let url = str_of(source, "url")?;
            Some(json!({
                "type": "image_url",
                "image_url": { "url": url }
            }))
        }
        _ => None,
    }
}

/// Normalise `tool_result.content`, which may be a string or an array of
/// blocks, into a plain string for OpenAI's `role: "tool"` message.
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| match str_of(b, "type") {
                Some("text") => str_of(b, "text").map(|t| t.to_string()),
                Some("image") => Some("[image omitted]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Push one Anthropic message, which may fan out into several OpenAI messages.
///
/// Anthropic content arrays freely mix text, images, `tool_use` and
/// `tool_result` inside a *single* turn. OpenAI has no such turn: tool results
/// must be their own `role: "tool"` message and tool calls must hang off an
/// assistant message. Order matters to the upstream model, so tool results go
/// first (they answer the preceding assistant turn), then text/images.
fn push_translated_message(messages: &mut Vec<Value>, msg: &Value) {
    let role = str_of(msg, "role").unwrap_or("user");
    let is_assistant = role == "assistant";

    let blocks = match msg.get("content") {
        Some(Value::String(s)) => {
            messages.push(json!({ "role": role, "content": s }));
            return;
        }
        Some(Value::Array(arr)) => arr.clone(),
        // `null`/missing content: emit an empty turn rather than dropping it,
        // so message ordering stays intact for the upstream model.
        _ => {
            messages.push(json!({ "role": role, "content": "" }));
            return;
        }
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut user_parts: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in &blocks {
        match str_of(block, "type") {
            Some("text") => {
                if let Some(t) = str_of(block, "text") {
                    text_parts.push(t.to_string());
                    // Only user turns need structured parts; assistant text
                    // collapses into a plain string.
                    if !is_assistant {
                        user_parts.push(json!({ "type": "text", "text": t }));
                    }
                }
            }
            Some("image") => {
                if !is_assistant {
                    if let Some(part) = anthropic_image_to_openai(block) {
                        user_parts.push(part);
                    }
                }
            }
            Some("tool_use") => {
                let name = str_of(block, "name").unwrap_or("unknown").to_string();
                let raw_id = str_of(block, "id").unwrap_or("").to_string();
                let id = if raw_id.is_empty() {
                    format!("toolu_{}", uuid_short())
                } else {
                    to_anthropic_tool_id(&raw_id)
                };
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());

                tool_calls.push(json!({
                    "id": to_openai_tool_id(&id),
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }));
            }
            Some("tool_result") => {
                let tool_use_id = str_of(block, "tool_use_id").unwrap_or("").to_string();
                let mut text = tool_result_text(block);
                if block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    text.insert_str(0, "[tool errored] ");
                }
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": to_openai_tool_id(&tool_use_id),
                    "content": text
                }));
            }
            // `thinking`, `redacted_thinking` and any vendor extension have no
            // OpenAI counterpart; dropping them is lossless for the model.
            _ => {}
        }
    }

    // 1. Tool results answer the previous assistant turn and must precede any
    //    new user text, otherwise the upstream model sees a dangling call.
    for tr in tool_results {
        messages.push(tr);
    }

    // 2. The assistant turn itself, carrying its tool calls.
    if is_assistant {
        let has_payload = !text_parts.is_empty() || !tool_calls.is_empty();
        if !has_payload {
            return;
        }
        let mut msg_out = Map::new();
        msg_out.insert("role".to_string(), json!("assistant"));
        if text_parts.is_empty() {
            msg_out.insert("content".to_string(), Value::Null);
        } else {
            msg_out.insert("content".to_string(), json!(text_parts.join("\n")));
        }
        if !tool_calls.is_empty() {
            msg_out.insert("tool_calls".to_string(), Value::Array(tool_calls));
        }
        messages.push(Value::Object(msg_out));
        return;
    }

    // 3. User turn: plain string when it is text-only, structured parts when
    //    images are involved.
    let text_only = user_parts.iter().all(|p| str_of(p, "type") == Some("text"));
    if user_parts.len() == 1 && text_only {
        messages.push(json!({ "role": "user", "content": text_parts.join("\n") }));
    } else if user_parts.is_empty() {
        if !text_parts.is_empty() {
            messages.push(json!({ "role": "user", "content": text_parts.join("\n") }));
        }
    } else {
        messages.push(json!({ "role": "user", "content": user_parts }));
    }
}

/// Convert an Anthropic `POST /v1/messages` body into an OpenAI
/// `POST /v1/chat/completions` body.
pub fn anthropic_to_opencode_request(body: &Value) -> Value {
    let model = str_of(body, "model").unwrap_or("").to_string();
    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(4096);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = system_text(body) {
        if !system.is_empty() {
            messages.push(json!({ "role": "system", "content": system }));
        }
    }

    if let Some(turns) = body.get("messages").and_then(Value::as_array) {
        for turn in turns {
            push_translated_message(&mut messages, turn);
        }
    }

    let mut req = Map::new();
    req.insert("model".to_string(), Value::String(model));
    req.insert("messages".to_string(), Value::Array(messages));
    req.insert("stream".to_string(), Value::Bool(stream));
    req.insert("max_tokens".to_string(), json!(max_tokens));

    // Sampling parameters. Anthropic and OpenAI agree on the names for these.
    for key in ["temperature", "top_p", "top_k"] {
        if let Some(v) = body.get(key) {
            req.insert(key.to_string(), v.clone());
        }
    }
    if let Some(stop) = body.get("stop_sequences") {
        req.insert("stop".to_string(), stop.clone());
    }
    if let Some(metadata) = body.get("metadata") {
        if let Some(uid) = str_of(metadata, "user_id") {
            req.insert("user".to_string(), json!(uid));
        }
    }

    // Tools.
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mut out: Vec<Value> = Vec::new();
        for tool in tools {
            let name = match str_of(tool, "name") {
                Some(n) if !n.is_empty() => n,
                _ => continue,
            };
            let mut function = Map::new();
            function.insert("name".to_string(), json!(name));
            if let Some(desc) = str_of(tool, "description") {
                function.insert("description".to_string(), json!(desc));
            }
            let schema = tool
                .get("input_schema")
                .cloned()
                .or_else(|| tool.get("parameters").cloned())
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            function.insert("parameters".to_string(), schema);

            let mut entry = Map::new();
            entry.insert("type".to_string(), json!("function"));
            entry.insert("function".to_string(), Value::Object(function));
            out.push(Value::Object(entry));
        }
        if !out.is_empty() {
            req.insert("tools".to_string(), Value::Array(out));
        }
    }

    // Tool choice. Only emit it when tools were declared, otherwise some
    // upstreams reject the request outright.
    if req.contains_key("tools") {
        if let Some(choice) = body.get("tool_choice") {
            let mapped = match str_of(choice, "type") {
                Some("auto") => json!("auto"),
                Some("any") => json!("required"),
                Some("none") => json!("none"),
                Some("tool") => match str_of(choice, "name") {
                    Some(name) => json!({
                        "type": "function",
                        "function": { "name": name }
                    }),
                    None => json!("auto"),
                },
                _ => json!("auto"),
            };
            req.insert("tool_choice".to_string(), mapped);
        }
    }

    Value::Object(req)
}

// ---------------------------------------------------------------------------
// Response: OpenAI -> Anthropic
// ---------------------------------------------------------------------------

/// Map OpenAI `usage` onto Anthropic's `input_tokens`/`output_tokens`.
///
/// Passing the upstream object through verbatim (the previous behaviour) makes
/// Claude Code report zero-token usage forever, because it reads the Anthropic
/// key names.
fn map_usage(usage: Option<&Value>) -> Value {
    let usage = match usage {
        Some(usage @ Value::Object(_)) => usage,
        // Missing or unexpected shape: report zeroes rather than a schema that
        // Claude Code would reject.
        _ => return json!({ "input_tokens": 0, "output_tokens": 0 }),
    };

    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("prompt_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("completion_tokens").and_then(Value::as_u64))
        .unwrap_or(0);

    let mut out = Map::new();
    out.insert("input_tokens".to_string(), json!(input));
    out.insert("output_tokens".to_string(), json!(output));

    // Prompt-cache accounting, if the upstream reports it.
    if let Some(v) = usage
        .get("cache_creation_input_tokens")
        .or_else(|| usage.get("cache_creation_tokens"))
    {
        out.insert("cache_creation_input_tokens".to_string(), v.clone());
    }
    if let Some(v) = usage
        .get("cache_read_input_tokens")
        .or_else(|| usage.get("cached_tokens"))
    {
        out.insert("cache_read_input_tokens".to_string(), v.clone());
    }

    Value::Object(out)
}

/// Map an OpenAI `finish_reason` onto an Anthropic `stop_reason`.
fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        "content_filter" => "end_turn",
        // "stop" and anything unrecognised terminate the turn normally.
        _ => "end_turn",
    }
}

/// Convert a complete OpenAI chat completion into an Anthropic message.
pub fn opencode_response_to_anthropic(body: &Value, model: &str) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut stop_reason = "end_turn";

    if let Some(choice) = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    {
        if let Some(message) = choice.get("message") {
            if let Some(text) = str_of(message, "content") {
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
            }
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let raw_id = str_of(call, "id").unwrap_or("");
                    let id = if raw_id.is_empty() {
                        format!("toolu_{}", uuid_short())
                    } else {
                        to_anthropic_tool_id(raw_id)
                    };
                    let function = call.get("function");
                    let name = function
                        .and_then(|f| str_of(f, "name"))
                        .unwrap_or("unknown")
                        .to_string();
                    let raw_args = function
                        .and_then(|f| str_of(f, "arguments"))
                        .unwrap_or("{}");
                    let input = serde_json::from_str::<Value>(raw_args)
                        .unwrap_or_else(|_| json!({ "_raw": raw_args }));

                    content.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input
                    }));
                }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            stop_reason = map_stop_reason(reason);
        }
    }

    // Anthropic rejects a message with an empty content array.
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }

    json!({
        "id": format!("msg_{}", uuid_short()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": map_usage(body.get("usage"))
    })
}

// ---------------------------------------------------------------------------
// Streaming: OpenAI SSE -> Anthropic SSE
// ---------------------------------------------------------------------------

/// Emits one complete Anthropic SSE frame.
fn frame(event: &str, data: Value) -> String {
    let payload = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
    format!("event: {}\ndata: {}\n\n", event, payload)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Tool,
}

/// Stateful OpenAI-SSE -> Anthropic-SSE rewriter.
///
/// Claude Code's Anthropic client refuses to render a stream that does not
/// begin with `message_start`, and it indexes content blocks by an explicit
/// counter that this translator owns. It also reassembles SSE lines, because a
/// single upstream chunk frequently contains several lines *or* half of one.
pub struct SseTranslator {
    /// Bytes of the current chunk not yet terminated by a newline.
    buffer: Vec<u8>,
    model: String,
    started: bool,
    closed: bool,
    /// Kind of the content block currently being written, if any.
    open_block: Option<BlockKind>,
    /// Index of the content block currently being written.
    open_index: i64,
    /// Next content block index to allocate.
    next_index: i64,
    /// Upstream `delta.tool_calls[].index` -> our content block index.
    tool_blocks: HashMap<i64, i64>,
    stop_reason: String,
    input_tokens: u64,
    output_tokens: u64,
}

impl SseTranslator {
    pub fn new(model: &str) -> Self {
        Self {
            buffer: Vec::new(),
            model: model.to_string(),
            started: false,
            closed: false,
            open_block: None,
            open_index: 0,
            next_index: 0,
            tool_blocks: HashMap::new(),
            stop_reason: "end_turn".to_string(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    /// Feed raw bytes from the upstream body; returns whatever Anthropic SSE
    /// frames became emit-able. Partial lines are buffered until completed.
    pub fn feed(&mut self, chunk: &[u8]) -> String {
        self.buffer.extend_from_slice(chunk);

        let mut out = String::new();
        while let Some(pos) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(&['\r', '\n'][..]);

            // SSE comments and framing fields carry no payload for us.
            if let Some(data) = line.strip_prefix("data:").map(str::trim_start) {
                out.push_str(&self.on_data(data));
            }
        }
        out
    }

    /// Emit the mandatory `message_start` preamble exactly once.
    fn ensure_started(&mut self) -> String {
        if self.started {
            return String::new();
        }
        self.started = true;
        frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": format!("msg_{}", uuid_short()),
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 }
                }
            }),
        )
    }

    fn close_open_block(&mut self) -> String {
        match self.open_block.take() {
            Some(_) => frame(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": self.open_index }),
            ),
            None => String::new(),
        }
    }

    /// Handle one complete SSE `data:` payload.
    fn on_data(&mut self, data: &str) -> String {
        if self.closed {
            return String::new();
        }
        if data == "[DONE]" {
            return self.close();
        }

        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            // Malformed or non-JSON frames are dropped rather than forwarded:
            // leaking raw OpenAI frames to an Anthropic client desyncs it.
            Err(_) => return String::new(),
        };

        if let Some(usage) = value.get("usage") {
            if let Some(u) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                self.input_tokens = u;
            }
            if let Some(u) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.output_tokens = u;
            }
        }

        let choice = match value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            Some(c) => c,
            None => return String::new(),
        };

        let mut out = self.ensure_started();

        if let Some(delta) = choice.get("delta") {
            if let Some(text) = str_of(delta, "content") {
                if !text.is_empty() {
                    if self.open_block != Some(BlockKind::Text) {
                        out.push_str(&self.close_open_block());
                        self.open_block = Some(BlockKind::Text);
                        self.open_index = self.next_index;
                        self.next_index += 1;
                        out.push_str(&frame(
                            "content_block_start",
                            json!({
                                "type": "content_block_start",
                                "index": self.open_index,
                                "content_block": { "type": "text", "text": "" }
                            }),
                        ));
                    }
                    out.push_str(&frame(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": self.open_index,
                            "delta": { "type": "text_delta", "text": text }
                        }),
                    ));
                }
            }

            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let upstream_index = call.get("index").and_then(Value::as_i64).unwrap_or(0);
                    let function = call.get("function");

                    let block_index = match self.tool_blocks.get(&upstream_index) {
                        Some(existing) => *existing,
                        None => {
                            // A new tool call starts a new content block.
                            out.push_str(&self.close_open_block());
                            let assigned = self.next_index;
                            self.next_index += 1;
                            self.tool_blocks.insert(upstream_index, assigned);
                            self.open_block = Some(BlockKind::Tool);
                            self.open_index = assigned;

                            let raw_id = str_of(call, "id").unwrap_or("");
                            let id = if raw_id.is_empty() {
                                format!("toolu_{}", uuid_short())
                            } else {
                                to_anthropic_tool_id(raw_id)
                            };
                            let name = function
                                .and_then(|f| str_of(f, "name"))
                                .unwrap_or("unknown");

                            out.push_str(&frame(
                                "content_block_start",
                                json!({
                                    "type": "content_block_start",
                                    "index": assigned,
                                    "content_block": {
                                        "type": "tool_use",
                                        "id": id,
                                        "name": name,
                                        "input": {}
                                    }
                                }),
                            ));
                            assigned
                        }
                    };

                    // Argument fragments stream in as partial JSON.
                    if let Some(args) = function.and_then(|f| str_of(f, "arguments")) {
                        if !args.is_empty() {
                            out.push_str(&frame(
                                "content_block_delta",
                                json!({
                                    "type": "content_block_delta",
                                    "index": block_index,
                                    "delta": { "type": "input_json_delta", "partial_json": args }
                                }),
                            ));
                        }
                    }
                }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            if !reason.is_empty() {
                self.stop_reason = map_stop_reason(reason).to_string();
            }
            if let Some(usage) = choice.get("usage") {
                if let Some(u) = usage.get("completion_tokens").and_then(Value::as_u64) {
                    self.output_tokens = u;
                }
            }
        }

        out
    }

    /// Terminate the stream: close any open block, emit `message_delta` with
    /// the final stop reason and usage, then `message_stop`.
    pub fn close(&mut self) -> String {
        if self.closed {
            return String::new();
        }
        self.closed = true;

        let mut out = self.ensure_started();
        out.push_str(&self.close_open_block());
        out.push_str(&frame(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": self.stop_reason,
                    "stop_sequence": Value::Null
                },
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": self.output_tokens
                }
            }),
        ));
        out.push_str(&frame("message_stop", json!({ "type": "message_stop" })));
        out
    }
}

/// Short, monotonic-enough identifier component. Not a UUID, but the Anthropic
/// `msg_`/`toolu_` prefixes only need uniqueness within a single response.
pub fn uuid_short() -> String {
    use std::hash::Hasher;
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut state = std::collections::hash_map::DefaultHasher::new();
    state.write_u64(duration.as_nanos() as u64);
    format!("{:016x}", state.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(text: &str) -> Vec<(String, Value)> {
        text.split("\n\n")
            .filter(|f| !f.trim().is_empty())
            .filter_map(|frame| {
                let mut event = String::new();
                let mut data = None;
                for line in frame.lines() {
                    if let Some(e) = line.strip_prefix("event: ") {
                        event = e.to_string();
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data = Some(serde_json::from_str::<Value>(d).ok()?);
                    }
                }
                Some((event, data?))
            })
            .collect()
    }

    #[test]
    fn request_maps_system_and_text() {
        let body = json!({
            "model": "glm-4.7-free",
            "max_tokens": 1024,
            "system": "be terse",
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let out = anthropic_to_opencode_request(&body);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][1]["content"], "hello");
        assert_eq!(out["max_tokens"], 1024);
        assert_eq!(out["stream"], false);
    }

    #[test]
    fn request_maps_tool_use_blocks_to_tool_calls() {
        let body = json!({
            "model": "m",
            "max_tokens": 16,
            "messages": [{
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "running it" },
                    { "type": "tool_use", "id": "call_abc", "name": "bash",
                      "input": { "command": "ls" } }
                ]
            }]
        });
        let out = anthropic_to_opencode_request(&body);
        let msg = &out["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "running it");
        assert_eq!(msg["tool_calls"][0]["id"], "call_abc");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            serde_json::from_str::<Value>(
                msg["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
            json!({ "command": "ls" })
        );
    }

    #[test]
    fn request_maps_tool_result_and_reorders() {
        let body = json!({
            "model": "m",
            "max_tokens": 16,
            "messages": [
                { "role": "user", "content": "go" },
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "call_1", "name": "bash", "input": {} }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "call_1", "content": "ok" },
                    { "type": "text", "text": "now what?" }
                ]}
            ]
        });
        let out = anthropic_to_opencode_request(&body);
        let msgs = out["messages"].as_array().unwrap();
        // user -> assistant(tool_call) -> tool -> user(text)
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_1");
        assert_eq!(msgs[2]["content"], "ok");
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(msgs[3]["content"], "now what?");
    }

    #[test]
    fn tool_ids_round_trip() {
        assert_eq!(to_anthropic_tool_id("call_9"), "toolu_call_9");
        assert_eq!(to_openai_tool_id("toolu_call_9"), "call_9");
        // Ids that already carry the prefix are left alone.
        assert_eq!(to_anthropic_tool_id("toolu_x"), "toolu_x");
    }

    #[test]
    fn response_maps_usage_keys() {
        let body = json!({
            "choices": [{
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 7 }
        });
        let out = opencode_response_to_anthropic(&body, "glm-4.7-free");
        assert_eq!(out["usage"]["input_tokens"], 11);
        assert_eq!(out["usage"]["output_tokens"], 7);
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "hi");
    }

    #[test]
    fn response_maps_tool_calls_with_toolu_prefix() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_z",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"a\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2 }
        });
        let out = opencode_response_to_anthropic(&body, "m");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["content"][0]["type"], "tool_use");
        assert_eq!(out["content"][0]["id"], "toolu_call_z");
        assert_eq!(out["content"][0]["input"]["path"], "a");
    }

    #[test]
    fn response_maps_length_to_max_tokens() {
        let body = json!({
            "choices": [{ "message": { "content": "x" }, "finish_reason": "length" }]
        });
        assert_eq!(
            opencode_response_to_anthropic(&body, "m")["stop_reason"],
            "max_tokens"
        );
    }

    #[test]
    fn stream_emits_message_start_first() {
        let mut t = SseTranslator::new("glm-4.7-free");
        let out = t.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n");
        let parsed = frames(&out);
        assert_eq!(parsed[0].0, "message_start");
        assert_eq!(parsed[1].0, "content_block_start");
        assert_eq!(parsed[2].0, "content_block_delta");
        assert_eq!(parsed[2].1["delta"]["text"], "he");
    }

    #[test]
    fn stream_reassembles_split_lines() {
        let mut t = SseTranslator::new("m");
        let payload = b"data: {\"choices\":[{\"delta\":{\"content\":\"split\"}}]}\n\n";
        // Cut the frame in the middle of the JSON payload.
        let (a, b) = payload.split_at(27);
        assert!(t.feed(a).is_empty(), "half a line must not emit anything");
        let out = t.feed(b);
        assert!(out.contains("\"text\":\"split\""));
    }

    #[test]
    fn stream_terminates_on_done() {
        let mut t = SseTranslator::new("m");
        // `[DONE]` itself closes the stream, so `feed` emits the terminator.
        let out = t.feed(b"data: [DONE]\n\n");
        let parsed = frames(&out);
        let events: Vec<&str> = parsed.iter().map(|(e, _)| e.as_str()).collect();
        assert!(events.contains(&"message_delta"), "{:?}", events);
        assert_eq!(*events.last().unwrap(), "message_stop");
        // close() after the fact is a no-op, and never emits a second
        // terminator.
        assert!(t.close().is_empty());
    }

    #[test]
    fn stream_terminates_when_upstream_ends_without_done() {
        // A dropped upstream connection must still produce a well-formed
        // Anthropic terminator, or the client hangs forever.
        let mut t = SseTranslator::new("m");
        let _ = t.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n");
        let out = t.close();
        let parsed = frames(&out);
        let events: Vec<&str> = parsed.iter().map(|(e, _)| e.as_str()).collect();
        // `message_start` was already emitted during `feed`; the flush owns the
        // closing half of the handshake.
        assert!(events.contains(&"content_block_stop"), "{:?}", events);
        assert!(events.contains(&"message_delta"), "{:?}", events);
        assert_eq!(*events.last().unwrap(), "message_stop");
    }

    #[test]
    fn stream_translates_tool_call_fragments() {
        let mut t = SseTranslator::new("m");
        let mut out = String::new();
        out.push_str(&t.feed(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\n",
        ));
        out.push_str(&t.feed(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"a\\\"\"}}]}}]}\n\n",
        ));
        out.push_str(
            &t.feed(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n"),
        );
        out.push_str(&t.feed(b"data: [DONE]\n\n"));

        let parsed = frames(&out);
        assert_eq!(parsed[0].0, "message_start");
        let start = parsed
            .iter()
            .find(|(e, _)| e == "content_block_start")
            .expect("tool block start");
        assert_eq!(start.1["content_block"]["type"], "tool_use");
        assert_eq!(start.1["content_block"]["id"], "toolu_call_a");
        assert_eq!(start.1["content_block"]["name"], "bash");

        let delta = parsed
            .iter()
            .find(|(e, d)| e == "content_block_delta" && d["delta"]["type"] == "input_json_delta")
            .expect("input_json_delta");
        assert_eq!(delta.1["delta"]["partial_json"], "{\"a\"");

        let stop = parsed
            .iter()
            .find(|(_, d)| d["delta"].get("stop_reason").is_some())
            .expect("message_delta");
        assert_eq!(stop.1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn stream_drops_malformed_frames() {
        let mut t = SseTranslator::new("m");
        assert!(t.feed(b"data: {not json}\n\n").is_empty());
        assert!(t.feed(b": keep-alive comment\n\n").is_empty());
    }
}
