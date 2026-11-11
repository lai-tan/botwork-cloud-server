// Copyright (c) 2026 Jehu Shaw. All rights reserved.

use serde_json::{json, Value};
use uuid::Uuid;

fn estimate_tokens_simple(text: &str) -> u64 {
    let mut ascii_count = 0u64;
    let mut non_ascii_count = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_count += 1;
        } else {
            non_ascii_count += 1;
        }
    }
    ascii_count / 4 + non_ascii_count / 2 + 1
}

/// Convert Anthropic Messages API request → OpenAI Chat Completions request
pub fn anthropic_to_chat_completions(req: &Value) -> Value {
    let mut chat_req = json!({});

    if let Some(model) = req.get("model") {
        chat_req["model"] = model.clone();
    }

    if let Some(max_tokens) = req.get("max_tokens") {
        chat_req["max_tokens"] = max_tokens.clone();
    }

    if let Some(temperature) = req.get("temperature") {
        chat_req["temperature"] = temperature.clone();
    }

    if let Some(top_p) = req.get("top_p") {
        chat_req["top_p"] = top_p.clone();
    }

    if let Some(stop) = req.get("stop_sequences") {
        chat_req["stop"] = stop.clone();
    }

    if let Some(stream) = req.get("stream") {
        chat_req["stream"] = stream.clone();
    }

    let mut messages = Vec::new();

    if let Some(system) = req.get("system") {
        let system_text = match system {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        };
        if !system_text.is_empty() {
            messages.push(json!({"role": "system", "content": system_text}));
        }
    }

    if let Some(msgs) = req.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let content = convert_content_to_string(msg.get("content"));
            messages.push(json!({"role": role, "content": content}));
        }
    }

    chat_req["messages"] = Value::Array(messages);

    chat_req
}

/// Convert OpenAI Chat Completions response → Anthropic Messages API response
pub fn chat_completions_to_anthropic(resp: &Value, req: &Value) -> Value {
    let msg_id = format!("msg_{}", Uuid::new_v4().simple());

    let content_text = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let finish_reason = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("stop");

    let stop_reason = map_finish_reason(finish_reason);

    let model = req
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("claude-sonnet-4-20250514");

    let (input_tokens, output_tokens) = if let Some(usage) = resp.get("usage").filter(|u| !u.is_null()) {
        let it = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let ot = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        (it, ot)
    } else {
        let mut input_text = String::new();
        if let Some(messages) = req.get("messages").and_then(|v| v.as_array()) {
            for msg in messages {
                if let Some(content) = msg.get("content") {
                    if let Some(s) = content.as_str() {
                        input_text.push_str(s);
                    } else if let Some(arr) = content.as_array() {
                        for part in arr {
                            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                input_text.push_str(t);
                            }
                        }
                    }
                }
            }
        }
        (estimate_tokens_simple(&input_text), estimate_tokens_simple(&content_text))
    };

    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": content_text}],
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    })
}

/// Convert OpenAI streaming SSE → Anthropic streaming SSE
pub fn convert_anthropic_sse_stream(
    byte_stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    req_body: Value,
) -> impl futures::Stream<Item = Result<String, std::io::Error>> + Send + 'static {
    use futures::StreamExt;

    let model = req_body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("claude-sonnet-4-20250514")
        .to_string();

    let max_tokens = req_body
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096);

    async_stream::stream! {
        let msg_id = format!("msg_{}", Uuid::new_v4().simple());

        let message_start = json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        yield Ok(format_sse_event("message_start", &message_start));

        let content_block_start = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        });
        yield Ok(format_sse_event("content_block_start", &content_block_start));

        let mut buffer = String::new();
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut finish_reason: String = String::new();
        let mut stream = Box::pin(byte_stream);
        let mut started = false;

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()));
                    return;
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk[..]));

            while let Some(pos) = buffer.find('\n') {
                let line: String = buffer.drain(..=pos).collect();
                let line = line.trim();

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        continue;
                    }

                    let chunk_json: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if !started {
                        started = true;
                    }

                    if let Some(usage) = chunk_json.get("usage") {
                        if let Some(pt) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
                            input_tokens = pt;
                        }
                        if let Some(ct) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                            output_tokens = ct;
                        }
                    }

                    if let Some(delta_text) = chunk_json
                        .get("choices")
                        .and_then(|c| c.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|choice| choice.get("delta"))
                        .and_then(|d| d.get("content"))
                        .and_then(|v| v.as_str())
                    {
                        if !delta_text.is_empty() {
                            let delta = json!({
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": {"type": "text_delta", "text": delta_text}
                            });
                            yield Ok(format_sse_event("content_block_delta", &delta));
                        }
                    }

                    if let Some(fr) = chunk_json
                        .get("choices")
                        .and_then(|c| c.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|choice| choice.get("finish_reason"))
                        .and_then(|v| v.as_str())
                    {
                        if !fr.is_empty() {
                            finish_reason = fr.to_string();
                        }
                    }
                }
            }
        }

        let content_block_stop = json!({
            "type": "content_block_stop",
            "index": 0
        });
        yield Ok(format_sse_event("content_block_stop", &content_block_stop));

        let stop_reason = if finish_reason.is_empty() {
            "end_turn".to_string()
        } else {
            map_finish_reason(&finish_reason)
        };

        let message_delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": output_tokens}
        });
        yield Ok(format_sse_event("message_delta", &message_delta));

        let message_stop = json!({"type": "message_stop"});
        yield Ok(format_sse_event("message_stop", &message_stop));
    }
}

fn format_sse_event(event_type: &str, data: &Value) -> String {
    format!("event: {event_type}\ndata: {data}\n\n")
}

fn map_finish_reason(reason: &str) -> String {
    match reason {
        "stop" => "end_turn".to_string(),
        "length" => "max_tokens".to_string(),
        "tool_calls" => "tool_use".to_string(),
        "content_filter" => "end_turn".to_string(),
        _ => "end_turn".to_string(),
    }
}

/// Map Anthropic model names to Agnes model names.
/// Unmapped `claude-*` models fall back to the default (agnes-3.0-flash).
/// Non-claude models are passed through unchanged.
pub fn map_model(model: &str) -> String {
    let default_model = std::env::var("AGNES_MODEL")
        .unwrap_or_else(|_| "agnes-3.0-flash".to_string());
    match model {
        "claude-sonnet-4-20250514" | "claude-3-5-sonnet-20241022" | "claude-3-5-sonnet-latest" => {
            default_model
        }
        "claude-3-opus-20240229" | "claude-3-opus-latest" => "agnes-2.5-pro".to_string(),
        "claude-3-haiku-20240307" | "claude-3-haiku-latest" => "agnes-2.0-flash".to_string(),
        _ if model.starts_with("claude-") => default_model,
        _ => model.to_string(),
    }
}

/// Convert a non-streaming Anthropic response into an SSE event stream.
/// Used when the client requests `stream=true` but the upstream API
/// does not support streaming (e.g. Agnes). The complete response is
/// split into the standard Anthropic SSE event sequence:
/// message_start → content_block_start → content_block_delta →
/// content_block_stop → message_delta → message_stop
pub fn anthropic_response_to_sse_stream(
    resp: Value,
) -> impl futures::Stream<Item = Result<String, std::io::Error>> + Send + 'static {
    async_stream::stream! {
        let msg_id = resp
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = resp
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let input_tokens = resp
            .get("usage")
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let message_start = json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": input_tokens, "output_tokens": 0}
            }
        });
        yield Ok(format_sse_event("message_start", &message_start));

        let content_block_start = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        });
        yield Ok(format_sse_event("content_block_start", &content_block_start));

        let text = resp
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !text.is_empty() {
            let delta = json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            });
            yield Ok(format_sse_event("content_block_delta", &delta));
        }

        let content_block_stop = json!({"type": "content_block_stop", "index": 0});
        yield Ok(format_sse_event("content_block_stop", &content_block_stop));

        let stop_reason = resp
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn");
        let output_tokens = resp
            .get("usage")
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| estimate_tokens_simple(text));
        let message_delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": output_tokens}
        });
        yield Ok(format_sse_event("message_delta", &message_delta));

        let message_stop = json!({"type": "message_stop"});
        yield Ok(format_sse_event("message_stop", &message_stop));
    }
}

fn convert_content_to_string(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match item_type {
                    "text" => {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    "tool_use" => {
                        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let input = item.get("input").map(|v| v.to_string()).unwrap_or_default();
                        parts.push(format!("\nTool call: {name}({input})"));
                    }
                    "tool_result" => {
                        let content = item
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        parts.push(format!("\n{content}"));
                    }
                    _ => {}
                }
            }
            parts.join("")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_to_chat_basic() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let chat = anthropic_to_chat_completions(&req);
        assert_eq!(
            chat.get("model").unwrap(),
            "claude-sonnet-4-20250514"
        );
        assert_eq!(chat.get("max_tokens").unwrap(), 1024);
        let messages = chat.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get("role").unwrap(), "user");
        assert_eq!(messages[0].get("content").unwrap(), "Hello");
    }

    #[test]
    fn test_anthropic_to_chat_with_system() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "system": "You are a helpful assistant.",
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let chat = anthropic_to_chat_completions(&req);
        let messages = chat.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].get("role").unwrap(), "system");
        assert_eq!(
            messages[0].get("content").unwrap(),
            "You are a helpful assistant."
        );
        assert_eq!(messages[1].get("role").unwrap(), "user");
    }

    #[test]
    fn test_anthropic_to_chat_with_system_array() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "system": [{"type": "text", "text": "Be helpful."}, {"type": "text", "text": "Be concise."}],
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let chat = anthropic_to_chat_completions(&req);
        let messages = chat.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages[0].get("role").unwrap(), "system");
        assert_eq!(
            messages[0].get("content").unwrap(),
            "Be helpful.Be concise."
        );
    }

    #[test]
    fn test_anthropic_to_chat_content_array() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello "}, {"type": "text", "text": "world"}]}
            ]
        });
        let chat = anthropic_to_chat_completions(&req);
        let messages = chat.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages[0].get("content").unwrap(), "Hello world");
    }

    #[test]
    fn test_anthropic_to_chat_with_stream() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "stream": true,
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let chat = anthropic_to_chat_completions(&req);
        assert_eq!(chat.get("stream").unwrap(), true);
    }

    #[test]
    fn test_anthropic_to_chat_with_stop_sequences() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "stop_sequences": ["END"],
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let chat = anthropic_to_chat_completions(&req);
        assert_eq!(chat.get("stop").unwrap(), &json!(["END"]));
    }

    #[test]
    fn test_chat_to_anthropic_basic() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let resp = json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hi there!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 25, "total_tokens": 35}
        });
        let anthropic = chat_completions_to_anthropic(&resp, &req);
        assert_eq!(anthropic.get("type").unwrap(), "message");
        assert_eq!(anthropic.get("role").unwrap(), "assistant");
        let content = anthropic.get("content").unwrap().as_array().unwrap();
        assert_eq!(content[0].get("type").unwrap(), "text");
        assert_eq!(content[0].get("text").unwrap(), "Hi there!");
        assert_eq!(anthropic.get("stop_reason").unwrap(), "end_turn");
        let usage = anthropic.get("usage").unwrap();
        assert_eq!(usage.get("input_tokens").unwrap(), 10);
        assert_eq!(usage.get("output_tokens").unwrap(), 25);
    }

    #[test]
    fn test_chat_to_anthropic_finish_reason_length() {
        let req = json!({"model": "test", "max_tokens": 100, "messages": []});
        let resp = json!({
            "choices": [{"finish_reason": "length", "message": {"content": "..."}}]
        });
        let anthropic = chat_completions_to_anthropic(&resp, &req);
        assert_eq!(anthropic.get("stop_reason").unwrap(), "max_tokens");
    }

    #[test]
    fn test_map_finish_reason() {
        assert_eq!(map_finish_reason("stop"), "end_turn");
        assert_eq!(map_finish_reason("length"), "max_tokens");
        assert_eq!(map_finish_reason("tool_calls"), "tool_use");
        assert_eq!(map_finish_reason("content_filter"), "end_turn");
        assert_eq!(map_finish_reason("unknown"), "end_turn");
    }

    #[test]
    fn test_format_sse_event() {
        let data = json!({"type": "message_stop"});
        let event = format_sse_event("message_stop", &data);
        assert!(event.starts_with("event: message_stop\n"));
        assert!(event.contains("data: {\"type\":\"message_stop\"}"));
        assert!(event.ends_with("\n\n"));
    }

    #[test]
    fn test_convert_content_to_string_string() {
        let content = Some(&json!("Hello world"));
        assert_eq!(convert_content_to_string(content), "Hello world");
    }

    #[test]
    fn test_convert_content_to_string_array() {
        let content = Some(&json!([{"type": "text", "text": "Hello "}, {"type": "text", "text": "world"}]));
        assert_eq!(convert_content_to_string(content), "Hello world");
    }

    #[test]
    fn test_convert_content_to_string_tool_use() {
        let content = Some(&json!([{"type": "tool_use", "name": "Bash", "input": {"command": "ls"}}]));
        let result = convert_content_to_string(content);
        assert!(result.contains("\nTool call: Bash"));
        assert!(result.contains("ls"));
    }

    #[test]
    fn test_convert_content_to_string_none() {
        assert_eq!(convert_content_to_string(None), "");
    }

    #[test]
    fn test_map_model_sonnet() {
        std::env::set_var("AGNES_MODEL", "agnes-3.0-flash");
        assert_eq!(map_model("claude-sonnet-4-20250514"), "agnes-3.0-flash");
        assert_eq!(map_model("claude-3-5-sonnet-20241022"), "agnes-3.0-flash");
        assert_eq!(map_model("claude-3-5-sonnet-latest"), "agnes-3.0-flash");
    }

    #[test]
    fn test_map_model_opus() {
        assert_eq!(map_model("claude-3-opus-20240229"), "agnes-2.5-pro");
        assert_eq!(map_model("claude-3-opus-latest"), "agnes-2.5-pro");
    }

    #[test]
    fn test_map_model_haiku() {
        assert_eq!(map_model("claude-3-haiku-20240307"), "agnes-2.0-flash");
        assert_eq!(map_model("claude-3-haiku-latest"), "agnes-2.0-flash");
    }

    #[test]
    fn test_map_model_unknown_claude_fallback() {
        std::env::set_var("AGNES_MODEL", "agnes-3.0-flash");
        assert_eq!(map_model("claude-some-future-model"), "agnes-3.0-flash");
    }

    #[test]
    fn test_map_model_non_claude_passthrough() {
        assert_eq!(map_model("gpt-4o"), "gpt-4o");
        assert_eq!(map_model("agnes-3.0-flash"), "agnes-3.0-flash");
    }

    #[tokio::test]
    async fn test_anthropic_response_to_sse_stream() {
        let resp = json!({
            "id": "msg_abc123",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello world"}],
            "model": "claude-sonnet-4-20250514",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        use futures::StreamExt;
        let stream = anthropic_response_to_sse_stream(resp);
        let events: Vec<String> = stream
            .map(|r| r.unwrap())
            .collect()
            .await;

        assert_eq!(events.len(), 6);
        assert!(events[0].starts_with("event: message_start\n"));
        assert!(events[0].contains("msg_abc123"));
        assert!(events[1].starts_with("event: content_block_start\n"));
        assert!(events[2].starts_with("event: content_block_delta\n"));
        assert!(events[2].contains("Hello world"));
        assert!(events[3].starts_with("event: content_block_stop\n"));
        assert!(events[4].starts_with("event: message_delta\n"));
        assert!(events[4].contains("end_turn"));
        assert!(events[5].starts_with("event: message_stop\n"));
    }
}