// Copyright (c) 2026 Jehu Shaw. All rights reserved.

use serde_json::Value;

/// 检测 API 类型。返回 true = Chat Completions API（需要适配器），false = Responses API。
///
/// 检测策略：
/// 1. 官方 OpenAI API（api.openai.com）→ false（完整支持 Responses API）
/// 2. 非官方 API → 向 `/responses` 发送含 `input_image` + `function_call_output` 的请求：
///    - 200 + 有 `output` 字段 → false（完整支持 Responses API）
///    - 其他状态码/超时/错误 → true（不完全支持，需要适配器）
pub async fn detect_api_type(base_url: &str, api_key: &str, model: &str) -> bool {
    if base_url.contains("api.openai.com") {
        return false;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(_) => return true,
    };

    let base = base_url.trim_end_matches('/');
    let auth = format!("Bearer {api_key}");
    let responses_url = format!("{base}/responses");

    let test_body = serde_json::json!({
        "model": model,
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGWMjU9CAAAAABJRU5ErkJggg=="},
                    {"type": "input_text", "text": "hi"}
                ]
            },
            {
                "type": "function_call",
                "call_id": "test_call",
                "name": "test",
                "arguments": "{}"
            },
            {
                "type": "function_call_output",
                "call_id": "test_call",
                "output": "ok"
            }
        ],
        "max_output_tokens": 1,
        "stream": false
    });

    let resp = match client
        .post(&responses_url)
        .header("Authorization", &auth)
        .header("Content-Type", "application/json")
        .json(&test_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return true,
    };

    if !resp.status().is_success() {
        return true;
    }

    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return true,
    };

    !body.get("output").is_some()
}