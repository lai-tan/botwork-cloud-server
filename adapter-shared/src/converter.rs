// Copyright (c) 2026 Jehu Shaw. All rights reserved.

use std::time::{SystemTime, UNIX_EPOCH};

use futures::stream::{Stream, StreamExt};
use serde_json::{json, Value};

pub fn responses_to_chat_completions(req: &Value) -> Value {
    let mut chat_req = json!({});

    if let Some(model) = req.get("model") {
        chat_req["model"] = model.clone();
    }

    let mut messages = Vec::new();

    if let Some(instructions) = req.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }

    if let Some(input) = req.get("input") {
        convert_input_to_messages(input, &mut messages);
    }

    chat_req["messages"] = json!(messages);

    if let Some(tools) = req.get("tools").and_then(|v| v.as_array()) {
        let chat_tools: Vec<Value> = tools.iter().filter_map(convert_tool).collect();
        if !chat_tools.is_empty() {
            chat_req["tools"] = json!(chat_tools);
        }
    }

    if let Some(tc) = req.get("tool_choice") {
        chat_req["tool_choice"] = tc.clone();
    }

    if let Some(temp) = req.get("temperature") {
        chat_req["temperature"] = temp.clone();
    }
    if let Some(top_p) = req.get("top_p") {
        chat_req["top_p"] = top_p.clone();
    }
    if let Some(max_tokens) = req.get("max_output_tokens") {
        chat_req["max_tokens"] = max_tokens.clone();
    }
    if let Some(effort) = req
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|v| v.as_str())
    {
        chat_req["reasoning_effort"] = json!(effort);
    }
    if let Some(stream) = req.get("stream") {
        chat_req["stream"] = stream.clone();
        if stream.as_bool() == Some(true) {
            chat_req["stream_options"] = json!({ "include_usage": true });
        }
    }
    if let Some(stop) = req.get("stop") {
        chat_req["stop"] = stop.clone();
    }
    if let Some(presence) = req.get("presence_penalty") {
        chat_req["presence_penalty"] = presence.clone();
    }
    if let Some(frequency) = req.get("frequency_penalty") {
        chat_req["frequency_penalty"] = frequency.clone();
    }
    if let Some(ptc) = req.get("parallel_tool_calls") {
        chat_req["parallel_tool_calls"] = ptc.clone();
    }
    if let Some(logprobs) = req.get("logprobs") {
        chat_req["logprobs"] = logprobs.clone();
    }
    if let Some(top_logprobs) = req.get("top_logprobs") {
        chat_req["top_logprobs"] = top_logprobs.clone();
    }
    if let Some(seed) = req.get("seed") {
        chat_req["seed"] = seed.clone();
    }

    chat_req
}

fn convert_input_to_messages(input: &Value, messages: &mut Vec<Value>) {
    if let Some(arr) = input.as_array() {
        for item in arr {
            convert_input_item(item, messages);
        }
    } else if let Some(text) = input.as_str() {
        messages.push(json!({"role": "user", "content": text}));
    }
}

fn convert_input_item(item: &Value, messages: &mut Vec<Value>) {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match item_type {
        "message" => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let chat_role = match role {
                "developer" | "system" => "system",
                "assistant" => "assistant",
                _ => "user",
            };

            if let Some(content) = item.get("content") {
                let converted_content = convert_content(content);
                messages.push(json!({"role": chat_role, "content": converted_content}));
            }
        }
        "function_call" => {
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
            let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");

            messages.push(json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }]
            }));
        }
        "function_call_output" => {
            let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
            let output = item.get("output").and_then(|v| v.as_str()).unwrap_or("");

            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output
            }));
        }
        "additional_tools" | "reasoning" => {
            // Skip — tools are handled separately; reasoning is not supported by chat completions
        }
        _ => {
            if let Some(content) = item.get("content") {
                let converted = convert_content(content);
                let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                messages.push(json!({"role": role, "content": converted}));
            }
        }
    }
}

fn convert_content(content: &Value) -> Value {
    if let Some(text) = content.as_str() {
        return json!(text);
    }

    if let Some(arr) = content.as_array() {
        let parts: Vec<Value> = arr
            .iter()
            .filter_map(|part| {
                let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match part_type {
                    "input_text" | "output_text" | "text" => {
                        part.get("text").map(|t| json!({"type": "text", "text": t}))
                    }
                    "input_image" => {
                        let url = part
                            .get("image_url")
                            .cloned()
                            .or_else(|| part.get("url").cloned());
                        url.map(|u| json!({"type": "image_url", "image_url": {"url": u}}))
                    }
                    _ => None,
                }
            })
            .collect();

        if parts.len() == 1 {
            if let Some(text) = parts[0].get("text") {
                return text.clone();
            }
        }
        return json!(parts);
    }

    json!(content)
}

fn convert_tool(tool: &Value) -> Option<Value> {
    let tool_type = tool.get("type").and_then(|v| v.as_str()).unwrap_or("function");

    match tool_type {
        "function" => {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let parameters = tool.get("parameters").cloned().unwrap_or(json!({}));

            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters
                }
            }))
        }
        "custom" => {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");

            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "patch": {
                                "type": "string",
                                "description": "The patch content to apply"
                            }
                        },
                        "required": ["patch"],
                        "additionalProperties": false
                    }
                }
            }))
        }
        _ => None,
    }
}

pub fn chat_completions_to_responses(resp: &Value, req: &Value) -> Value {
    let model = req.get("model").and_then(|v| v.as_str()).unwrap_or("unknown");
    let resp_id = format!("resp_{}", uuid_v4());

    let mut output = Vec::new();
    let mut end_turn = true;

    if let Some(choices) = resp.get("choices").and_then(|v| v.as_array()) {
        for choice in choices {
            if let Some(message) = choice.get("message") {
                let reasoning_content = message
                    .get("reasoning_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if !reasoning_content.is_empty() {
                    output.push(json!({
                        "type": "reasoning",
                        "id": format!("rs_{}", uuid_v4()),
                        "summary": [],
                        "content": [{"type": "summary_text", "text": reasoning_content}]
                    }));
                }

                let content = message
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if !content.is_empty() {
                    output.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content}]
                    }));
                }

                if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tool_calls {
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let function = tc.get("function");
                        let name = function.and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                        let arguments = function
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");

                        let final_args = if name == "apply_patch" {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(arguments) {
                                if let Some(patch) = parsed.get("patch").and_then(|v| v.as_str()) {
                                    patch.to_string()
                                } else {
                                    arguments.to_string()
                                }
                            } else {
                                arguments.to_string()
                            }
                        } else {
                            arguments.to_string()
                        };

                        output.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": final_args
                        }));
                    }
                }
            }

            if let Some(finish_reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                end_turn = finish_reason == "stop";
            }
        }
    }

    let usage = if let Some(u) = resp.get("usage").filter(|u| !u.is_null()) {
        convert_chat_usage_to_responses_usage(u)
    } else {
        let input_text = extract_input_text_from_req(req);
        let mut output_text = String::new();
        if let Some(choices) = resp.get("choices").and_then(|v| v.as_array()) {
            for choice in choices {
                if let Some(message) = choice.get("message") {
                    if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
                        output_text.push_str(content);
                    }
                    if let Some(reasoning) = message.get("reasoning_content").and_then(|v| v.as_str()) {
                        output_text.push_str(reasoning);
                    }
                }
            }
        }
        estimate_usage_from_text(&input_text, &output_text)
    };

    json!({
        "id": resp_id,
        "object": "response",
        "created_at": unix_timestamp(),
        "model": model,
        "status": "completed",
        "output": output,
        "usage": usage,
        "end_turn": end_turn
    })
}

pub fn convert_sse_stream(
    upstream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    req_body: Value,
) -> impl Stream<Item = Result<String, std::io::Error>> + Send + 'static {
    async_stream::stream! {
        let model = req_body.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let resp_id = format!("resp_{}", uuid_v4());
        let created_at = unix_timestamp();

        let created_event = json!({
            "type": "response.created",
            "response": {
                "id": resp_id,
                "object": "response",
                "created_at": created_at,
                "model": model,
                "status": "in_progress",
                "output": []
            }
        });
        yield Ok(format_sse_event("response.created", &created_event));

        let in_progress_event = json!({
            "type": "response.in_progress",
            "response": {"id": resp_id, "status": "in_progress"}
        });
        yield Ok(format_sse_event("response.in_progress", &in_progress_event));

        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut reasoning_text = String::new();
        let mut tool_calls: Vec<ToolCallAccumulator> = Vec::new();
        let mut usage: Option<Value> = None;
        let mut finish_reason_str: Option<String> = None;

        let mut reasoning_item_added = false;
        let mut reasoning_item_id: Option<String> = None;
        let mut message_item_added = false;
        let mut tool_item_added = false;
        let mut reasoning_index: u32 = 0;
        let mut message_index: u32 = 0;
        let mut tool_index: u32 = 0;

        let mut upstream = std::pin::pin!(upstream);

        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    yield Err(std::io::Error::other(format!("upstream error: {e}")));
                    return;
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim().to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        break;
                    }

                    let data_val: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if let Some(u) = data_val.get("usage") {
                        if !u.is_null() {
                            usage = Some(convert_chat_usage_to_responses_usage(u));
                        }
                    }

                    if let Some(choices) = data_val.get("choices").and_then(|v| v.as_array()) {
                        for choice in choices {
                            if let Some(delta) = choice.get("delta") {

                                if let Some(rc) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                                    if !rc.is_empty() {
                                        if !reasoning_item_added {
                                            reasoning_index = 0;
                                            message_index = 1;
                                            tool_index = 2;
                                            let reasoning_id = format!("rs_{}", uuid_v4());
                                            let reasoning_added = json!({
                                                "type": "response.output_item.added",
                                                "output_index": reasoning_index,
                                                "item": {
                                                    "type": "reasoning",
                                                    "id": reasoning_id,
                                                    "summary": []
                                                }
                                            });
                                            yield Ok(format_sse_event("response.output_item.added", &reasoning_added));
                                            let part_added = json!({
                                                "type": "response.reasoning_summary_part.added",
                                                "item_id": reasoning_id,
                                                "output_index": reasoning_index,
                                                "summary_index": 0
                                            });
                                            yield Ok(format_sse_event("response.reasoning_summary_part.added", &part_added));
                                            reasoning_item_id = Some(reasoning_id);
                                            reasoning_item_added = true;
                                        }
                                        reasoning_text.push_str(rc);
                                        let reasoning_delta = json!({
                                            "type": "response.reasoning_summary_text.delta",
                                            "output_index": reasoning_index,
                                            "summary_index": 0,
                                            "delta": rc
                                        });
                                        yield Ok(format_sse_event("response.reasoning_summary_text.delta", &reasoning_delta));
                                    }
                                }

                                if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                                    if !content.is_empty() {
                                        let cleaned = if content.trim().is_empty() {
                                            content.to_string()
                                        } else {
                                            content.trim_end_matches('\n').to_string()
                                        };
                                        if !cleaned.is_empty() {
                                            if !message_item_added {
                                                if !reasoning_item_added {
                                                    message_index = 0;
                                                    tool_index = 1;
                                                }
                                                let msg_added = json!({
                                                    "type": "response.output_item.added",
                                                    "output_index": message_index,
                                                    "item": {
                                                        "type": "message",
                                                        "role": "assistant",
                                                        "status": "in_progress",
                                                        "content": []
                                                    }
                                                });
                                                yield Ok(format_sse_event("response.output_item.added", &msg_added));
                                                message_item_added = true;
                                            }
                                            full_text.push_str(&cleaned);
                                            let delta_event = json!({
                                                "type": "response.output_text.delta",
                                                "output_index": message_index,
                                                "content_index": 0,
                                                "delta": cleaned
                                            });
                                            yield Ok(format_sse_event("response.output_text.delta", &delta_event));
                                        }
                                    }
                                }

                                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                                    for tc in tcs {
                                        let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                                        while tool_calls.len() <= idx {
                                            tool_calls.push(ToolCallAccumulator::default());
                                        }

                                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                            tool_calls[idx].id = id.to_string();
                                        }
                                        if let Some(func) = tc.get("function") {
                                            if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                                tool_calls[idx].name = name.to_string();
                                            }
                                            if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                                tool_calls[idx].arguments.push_str(args);

                                                if !tool_item_added {
                                                    if !reasoning_item_added && !message_item_added {
                                                        tool_index = 1;
                                                    } else if reasoning_item_added && !message_item_added {
                                                        tool_index = 1;
                                                    }
                                                    let fc_added = json!({
                                                        "type": "response.output_item.added",
                                                        "output_index": tool_index,
                                                        "item": {
                                                            "type": "function_call",
                                                            "call_id": tool_calls[idx].id,
                                                            "name": tool_calls[idx].name,
                                                            "arguments": "",
                                                            "status": "in_progress"
                                                        }
                                                    });
                                                    yield Ok(format_sse_event("response.output_item.added", &fc_added));
                                                    tool_item_added = true;
                                                }

                                                let args_delta = json!({
                                                    "type": "response.function_call_arguments.delta",
                                                    "output_index": tool_index,
                                                    "delta": args
                                                });
                                                yield Ok(format_sse_event("response.function_call_arguments.delta", &args_delta));
                                            }
                                        }
                                    }
                                }
                            }

                            if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                                if !fr.is_empty() && fr != "null" {
                                    finish_reason_str = Some(fr.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut output = Vec::new();

        if reasoning_item_added && !reasoning_text.is_empty() {
            let reasoning_done = json!({
                "type": "response.reasoning_summary_text.done",
                "output_index": reasoning_index,
                "summary_index": 0,
                "item_id": reasoning_item_id.clone().unwrap_or_else(|| format!("rs_{}", uuid_v4())),
                "text": reasoning_text
            });
            yield Ok(format_sse_event("response.reasoning_summary_text.done", &reasoning_done));

            let reasoning_item = json!({
                "type": "reasoning",
                "id": reasoning_item_id.clone().unwrap_or_else(|| format!("rs_{}", uuid_v4())),
                "summary": [],
                "content": [{"type": "summary_text", "text": reasoning_text}]
            });
            let item_done = json!({
                "type": "response.output_item.done",
                "output_index": reasoning_index,
                "item": reasoning_item
            });
            yield Ok(format_sse_event("response.output_item.done", &item_done));
            output.push(json!({
                "type": "reasoning",
                "id": reasoning_item_id.clone().unwrap_or_else(|| format!("rs_{}", uuid_v4())),
                "summary": [],
                "content": [{"type": "summary_text", "text": reasoning_text}]
            }));
        }

        if message_item_added && !full_text.is_empty() {
            let message_item = json!({
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": full_text}]
            });
            let item_done = json!({
                "type": "response.output_item.done",
                "output_index": message_index,
                "item": message_item
            });
            yield Ok(format_sse_event("response.output_item.done", &item_done));
            output.push(json!({
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": full_text}]
            }));
        }

        if tool_item_added {
            for tc in &tool_calls {
                let tc_done = json!({
                    "type": "response.output_item.done",
                    "output_index": tool_index,
                    "item": {
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.name,
                        "arguments": tc.arguments,
                        "status": "completed"
                    }
                });
                yield Ok(format_sse_event("response.output_item.done", &tc_done));
                output.push(json!({
                    "type": "function_call",
                    "call_id": tc.id,
                    "name": tc.name,
                    "arguments": tc.arguments,
                    "status": "completed"
                }));
            }
        }

        let is_incomplete = matches!(finish_reason_str.as_deref(), Some("length") | Some("content_filter"));
        let end_turn = match finish_reason_str.as_deref() {
            Some("stop") => true,
            Some("tool_calls") => false,
            _ => !is_incomplete,
        };

        if is_incomplete {
            let reason = match finish_reason_str.as_deref() {
                Some("length") => "max_output_tokens",
                Some("content_filter") => "content_filter",
                _ => "unknown",
            };
            let incomplete = json!({
                "type": "response.incomplete",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "created_at": created_at,
                    "model": model,
                    "status": "incomplete",
                    "output": output,
                    "incomplete_details": { "reason": reason }
                }
            });
            yield Ok(format_sse_event("response.incomplete", &incomplete));
        } else {
            let mut completed = json!({
                "type": "response.completed",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "created_at": created_at,
                    "model": model,
                    "status": "completed",
                    "output": output,
                    "end_turn": end_turn
                }
            });
            if usage.is_none() {
                let input_text = extract_input_text_from_req(&req_body);
                let mut output_text = full_text.clone();
                output_text.push_str(&reasoning_text);
                usage = Some(estimate_usage_from_text(&input_text, &output_text));
            }
            if let Some(u) = &usage {
                completed["response"]["usage"] = u.clone();
            }
            yield Ok(format_sse_event("response.completed", &completed));
        }
    }
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

fn estimate_tokens(text: &str) -> i64 {
    let mut ascii_count = 0i64;
    let mut non_ascii_count = 0i64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_count += 1;
        } else {
            non_ascii_count += 1;
        }
    }
    ascii_count / 4 + non_ascii_count / 2 + 1
}

fn estimate_usage_from_text(input_text: &str, output_text: &str) -> Value {
    let input_tokens = estimate_tokens(input_text);
    let output_tokens = estimate_tokens(output_text);
    let total_tokens = input_tokens + output_tokens;
    json!({
        "input_tokens": input_tokens,
        "input_tokens_details": { "cached_tokens": 0 },
        "output_tokens": output_tokens,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": total_tokens
    })
}

fn extract_input_text_from_req(req: &Value) -> String {
    let mut text = String::new();
    if let Some(messages) = req.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            if let Some(content) = msg.get("content") {
                if let Some(s) = content.as_str() {
                    text.push_str(s);
                } else if let Some(arr) = content.as_array() {
                    for part in arr {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            text.push_str(t);
                        }
                    }
                }
            }
        }
    }
    if let Some(input) = req.get("input") {
        if let Some(s) = input.as_str() {
            text.push_str(s);
        } else if let Some(arr) = input.as_array() {
            for item in arr {
                if let Some(content) = item.get("content") {
                    if let Some(s) = content.as_str() {
                        text.push_str(s);
                    } else if let Some(arr2) = content.as_array() {
                        for part in arr2 {
                            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                text.push_str(t);
                            }
                        }
                    }
                }
            }
        }
    }
    text
}

fn convert_chat_usage_to_responses_usage(usage: &Value) -> Value {
    let prompt_tokens = usage.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let completion_tokens = usage.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let total_tokens = usage.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(prompt_tokens + completion_tokens);

    let cached_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let reasoning_tokens = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    json!({
        "input_tokens": prompt_tokens,
        "input_tokens_details": { "cached_tokens": cached_tokens },
        "output_tokens": completion_tokens,
        "output_tokens_details": { "reasoning_tokens": reasoning_tokens },
        "total_tokens": total_tokens
    })
}


fn format_sse_event(event_type: &str, data: &Value) -> String {
    format!("event: {event_type}\ndata: {data}\n\n")
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

pub fn passthrough_sse_stream(
    upstream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    _req_body: Value,
) -> impl Stream<Item = Result<String, std::io::Error>> + Send + 'static {
    async_stream::stream! {
        let mut upstream = std::pin::pin!(upstream);
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => {
                    yield Ok(String::from_utf8_lossy(&bytes).into_owned());
                }
                Err(e) => {
                    yield Err(std::io::Error::other(format!("upstream error: {e}")));
                    return;
                }
            }
        }
    }
}

use bytes::Bytes;
#[cfg(test)]
mod reasoning_effort_tests {
    use super::responses_to_chat_completions;
    use serde_json::json;

    #[test]
    fn maps_reasoning_effort_to_chat_request() {
        let req = json!({
            "model": "test-model",
            "input": "hi",
            "reasoning": {"effort": "high", "summary": "auto"}
        });
        let chat = responses_to_chat_completions(&req);
        assert_eq!(chat["reasoning_effort"], json!("high"));
        assert!(chat.get("messages").is_some());
    }

    #[test]
    fn omits_reasoning_effort_when_absent() {
        let req = json!({
            "model": "test-model",
            "input": "hi"
        });
        let chat = responses_to_chat_completions(&req);
        assert!(chat.get("reasoning_effort").is_none());
    }

    #[test]
    fn omits_reasoning_effort_when_only_summary() {
        let req = json!({
            "model": "test-model",
            "input": "hi",
            "reasoning": {"summary": "auto"}
        });
        let chat = responses_to_chat_completions(&req);
        assert!(chat.get("reasoning_effort").is_none());
    }
}
