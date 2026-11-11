// Copyright (c) 2026 Jehu Shaw. All rights reserved.

use crate::converter;
use crate::anthropic_converter;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::oneshot;

pub struct ApiAdapter {
    port: u16,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

/// 多上游路由条目（DESIGN-ChatModelSelector §9.3 层 2「远端 adapter 动态多上游路由」）：
/// 本地经 env `BOTWORK_PROVIDERS` 传全 provider 表，远端按请求体 `model` 字段路由。
pub struct UpstreamProvider {
    pub base_url: String,
    pub api_key: String,
    /// detect_api_type 结果：true = Chat Completions（走 Responses→Chat 转换）
    pub is_chat: bool,
    /// 该上游服务的模型名列表（路由键；同名模型先注册者优先，已知限制）
    pub models: Vec<String>,
}

struct UpstreamRoute {
    base_url: String,
    api_key: String,
    is_chat: AtomicBool,
    models: Vec<String>,
}

impl ApiAdapter {
    pub async fn start(
        real_base_url: String,
        api_key: String,
        use_chat_completions: bool,
        port: Option<u16>,
    ) -> Result<Self, String> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| format!("failed to build http client: {e}"))?;

        let state = Arc::new(AdapterState {
            real_base_url: real_base_url.trim_end_matches('/').to_string(),
            api_key,
            client,
            use_chat_completions: AtomicBool::new(use_chat_completions),
            multi_upstreams: None,
        });

        Self::spawn_server(state, port).await
    }

    /// 多上游模式：codex 的 base_url 指向本 adapter 单一入口，
    /// 请求按 `model` 字段路由到对应上游（含 Authorization 替换）。
    pub async fn start_multi(
        upstreams: Vec<UpstreamProvider>,
        port: Option<u16>,
    ) -> Result<Self, String> {
        if upstreams.is_empty() {
            return Err("multi-upstream adapter requires at least one provider".to_string());
        }
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| format!("failed to build http client: {e}"))?;

        let mut seen = std::collections::HashSet::new();
        let routes: Vec<UpstreamRoute> = upstreams
            .into_iter()
            .map(|u| {
                for m in &u.models {
                    if !seen.insert(m.clone()) {
                        eprintln!("[adapter] multi-upstream: model {m} registered by multiple providers, first wins");
                    }
                }
                UpstreamRoute {
                    base_url: u.base_url.trim_end_matches('/').to_string(),
                    api_key: u.api_key,
                    is_chat: AtomicBool::new(u.is_chat),
                    models: u.models,
                }
            })
            .collect();

        let state = Arc::new(AdapterState {
            // 单上游兼容字段（multi 模式 handle_responses 不使用）
            real_base_url: routes[0].base_url.clone(),
            api_key: routes[0].api_key.clone(),
            client,
            use_chat_completions: AtomicBool::new(routes[0].is_chat.load(Ordering::Relaxed)),
            multi_upstreams: Some(routes),
        });

        Self::spawn_server(state, port).await
    }

    async fn spawn_server(
        state: Arc<AdapterState>,
        port: Option<u16>,
    ) -> Result<Self, String> {
        let app = Router::new()
            .route("/v1/responses", post(handle_responses))
            .route("/v1/messages", post(handle_messages))
            .route("/hello", get(|| async { "ok" }))
            .route("/health", get(|| async { "ok" }))
            .with_state(state);

        let bind_addr = match port {
            Some(p) => format!("127.0.0.1:{p}"),
            None => "127.0.0.1:0".to_string(),
        };
        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| format!("failed to bind {bind_addr}: {e}"))?;
        let port = listener
            .local_addr()
            .map(|a| a.port())
            .map_err(|e| format!("failed to get local addr: {e}"))?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .ok();
        });

        Ok(Self {
            port,
            shutdown_tx: Some(shutdown_tx),
        })
    }

    pub fn local_base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// 不含 `/v1` 前缀的 base URL，供 Claude Code 使用。
    /// Claude Code SDK 会在 base URL 后追加 `/v1/messages`，
    /// 而 adapter 路由是 `/v1/messages`，所以 base URL 不能再含 `/v1`。
    pub fn local_base_url_no_v1(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ApiAdapter {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

struct AdapterState {
    real_base_url: String,
    api_key: String,
    client: Client,
    use_chat_completions: AtomicBool,
    /// 多上游模式（批次 4）：Some 时 /v1/responses 按请求 `model` 字段路由；
    /// /v1/messages（anthropic 路径）维持首上游单一路由。
    multi_upstreams: Option<Vec<UpstreamRoute>>,
}

/// 多上游路由解析：按请求 model 名查命中 route（先注册者优先），
/// 未命中回退首条（与单上游行为等价——未知模型仍走默认网关）。
fn resolve_multi_route<'a>(
    routes: &'a [UpstreamRoute],
    model: Option<&str>,
) -> &'a UpstreamRoute {
    if let Some(m) = model {
        for r in routes {
            if r.models.iter().any(|x| x == m) {
                return r;
            }
        }
        eprintln!("[adapter] multi-upstream: model {m} not in any provider route, falling back to first");
    }
    &routes[0]
}

fn build_forwarded_request(
    client: &Client,
    api_key: &str,
    headers: &HeaderMap,
    url: &str,
    body: &Value,
) -> reqwest::RequestBuilder {
    let mut fwd_req = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Accept-Encoding", "identity");

    for (key, value) in headers.iter() {
        let key = key.as_str();
        if key == "content-type" || key == "host" || key == "content-length" || key == "authorization" || key == "x-api-key" || key == "anthropic-version" {
            continue;
        }
        fwd_req = fwd_req.header(key, value.clone());
    }

    fwd_req.json(body)
}

async fn handle_responses(
    State(state): State<Arc<AdapterState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let mut req_body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")).into_response();
        }
    };

    if let Some(tools) = req_body.get_mut("tools").and_then(|v| v.as_array_mut()) {
        tools.retain(|t| {
            let tt = t.get("type").and_then(|v| v.as_str()).unwrap_or("function");
            matches!(tt, "function" | "web_search_preview" | "code_interpreter" | "mcp")
        });
    }

    let is_stream = req_body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    // 批次 4 多上游路由：按请求 model 名解析目标上游（base_url/api_key/is_chat 三元组）。
    // 未启用多上游时退化为单上游全局字段（现状行为不变）。
    let req_model = req_body
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from);
    let target: (String, String, bool) = match &state.multi_upstreams {
        Some(routes) => {
            let r = resolve_multi_route(routes, req_model.as_deref());
            (
                r.base_url.clone(),
                r.api_key.clone(),
                r.is_chat.load(Ordering::Relaxed),
            )
        }
        None => (
            state.real_base_url.clone(),
            state.api_key.clone(),
            state.use_chat_completions.load(Ordering::Relaxed),
        ),
    };
    let (base_url, api_key, use_chat) = target;

    // API 类型动态检测翻转目标：多上游翻命中 route，单上游翻全局
    let flip_to_chat = |state: &AdapterState, model: Option<&str>| {
        match &state.multi_upstreams {
            Some(routes) => {
                let r = resolve_multi_route(routes, model);
                r.is_chat.store(true, Ordering::Relaxed);
            }
            None => state.use_chat_completions.store(true, Ordering::Relaxed),
        }
    };

    if !use_chat {
        let responses_url = format!("{}/responses", base_url);
        let fwd_req =
            build_forwarded_request(&state.client, &api_key, &headers, &responses_url, &req_body);

        match fwd_req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let code = status.as_u16();

                if code == 404 {
                    flip_to_chat(&state, req_model.as_deref());
                } else if code == 400 || code == 422 || code == 500 {
                    let body_text = resp.text().await.unwrap_or_default();
                    if body_text.contains("ChatCompletionRequest")
                        || body_text.contains("chat_completion")
                        || body_text.contains("ResponseInput")
                        || body_text.contains("json_parse_error")
                        || body_text.contains("Invalid JSON data")
                    {
                        flip_to_chat(&state, req_model.as_deref());
                    } else {
                        return (
                            status,
                            [("Content-Type", "application/json")],
                            body_text,
                        )
                            .into_response();
                    }
                } else {
                    return proxy_response(resp, is_stream, &req_body).await;
                }
            }
            Err(_) => {
                flip_to_chat(&state, req_model.as_deref());
            }
        }
    }

    let chat_req = converter::responses_to_chat_completions(&req_body);

    let chat_url = format!("{}/chat/completions", base_url);
    let fwd_req = build_forwarded_request(&state.client, &api_key, &headers, &chat_url, &chat_req);

    let resp = match fwd_req.send().await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response();
        }
    };

    proxy_chat_response(resp, is_stream, &req_body).await
}

/// 如果 IMAGE_GEN_API_KEY/URL/MODEL 环境变量存在，
/// 向 Chat Completions 请求注入 image_generation 工具定义。
fn inject_image_gen_tool(chat_req: &mut Value) {
    let (Ok(_key), Ok(_url), Ok(_model)) = (
        std::env::var("IMAGE_GEN_API_KEY"),
        std::env::var("IMAGE_GEN_API_URL"),
        std::env::var("IMAGE_GEN_MODEL"),
    ) else {
        return;
    };

    let tool_def = json!({
        "type": "function",
        "function": {
            "name": "image_generation",
            "description": "Generate an image from a text prompt. Use this when the user asks to create, draw, paint, or generate an image.",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "A text description of the image to generate."
                    },
                    "size": {
                        "type": "string",
                        "enum": ["1024x1024", "1792x1024", "1024x1792"],
                        "default": "1024x1024"
                    }
                },
                "required": ["prompt"]
            }
        }
    });

    match chat_req.get_mut("tools").and_then(|v| v.as_array_mut()) {
        Some(arr) => arr.push(tool_def),
        None => {
            chat_req["tools"] = json!([tool_def]);
        }
    }
}

/// 如果 VIDEO_GEN_API_KEY/URL/MODEL 环境变量存在，
/// 向 Chat Completions 请求注入 video_generation 工具定义。
fn inject_video_gen_tool(chat_req: &mut Value) {
    let (Ok(_key), Ok(_url), Ok(_model)) = (
        std::env::var("VIDEO_GEN_API_KEY"),
        std::env::var("VIDEO_GEN_API_URL"),
        std::env::var("VIDEO_GEN_MODEL"),
    ) else {
        return;
    };

    let tool_def = json!({
        "type": "function",
        "function": {
            "name": "video_generation",
            "description": "Generate a video from a text prompt. Use this when the user asks to create, generate, or make a video.",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "A text description of the video to generate."
                    },
                    "seconds": {
                        "type": "string",
                        "enum": ["4", "5", "6", "7", "8", "9", "10", "11", "12"],
                        "default": "5",
                        "description": "Video duration in seconds (4-12)."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["text", "keyframe", "reference"],
                        "default": "text"
                    },
                    "aspect_ratio": {
                        "type": "string",
                        "enum": ["16:9", "9:16", "1:1", "4:3", "3:4", "21:9"],
                        "default": "16:9"
                    }
                },
                "required": ["prompt"]
            }
        }
    });

    match chat_req.get_mut("tools").and_then(|v| v.as_array_mut()) {
        Some(arr) => arr.push(tool_def),
        None => {
            chat_req["tools"] = json!([tool_def]);
        }
    }
}

/// 从 Chat Completions 响应中提取 image_generation tool_calls。
/// 返回 (call_id, prompt, size) 列表。
fn extract_image_gen_tool_calls(resp: &Value) -> Vec<(String, String, String)> {
    let Some(choices) = resp.get("choices").and_then(|c| c.as_array()) else {
        return vec![];
    };
    let Some(choice) = choices.first() else {
        return vec![];
    };
    let Some(tool_calls) = choice
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    else {
        return vec![];
    };

    tool_calls
        .iter()
        .filter_map(|tc| {
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())?;
            if name != "image_generation" {
                return None;
            }
            let call_id = tc.get("id").and_then(|v| v.as_str())?.to_string();
            let args_str = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())?;
            let args: Value = serde_json::from_str(args_str).ok()?;
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let size = args
                .get("size")
                .and_then(|v| v.as_str())
                .unwrap_or("1024x1024")
                .to_string();
            Some((call_id, prompt, size))
        })
        .collect()
}

/// 从 Chat Completions 响应中提取 video_generation tool_calls。
/// 返回 (call_id, prompt, seconds, mode, aspect_ratio) 列表。
fn extract_video_gen_tool_calls(resp: &Value) -> Vec<(String, String, String, String, String)> {
    let Some(choices) = resp.get("choices").and_then(|c| c.as_array()) else {
        return vec![];
    };
    let Some(choice) = choices.first() else {
        return vec![];
    };
    let Some(tool_calls) = choice
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    else {
        return vec![];
    };

    tool_calls
        .iter()
        .filter_map(|tc| {
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())?;
            if name != "video_generation" {
                return None;
            }
            let call_id = tc.get("id").and_then(|v| v.as_str())?.to_string();
            let args_str = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())?;
            let args: Value = serde_json::from_str(args_str).ok()?;
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let seconds = args
                .get("seconds")
                .and_then(|v| v.as_str())
                .unwrap_or("5")
                .to_string();
            let mode = args
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("text")
                .to_string();
            let aspect_ratio = args
                .get("aspect_ratio")
                .and_then(|v| v.as_str())
                .unwrap_or("16:9")
                .to_string();
            Some((call_id, prompt, seconds, mode, aspect_ratio))
        })
        .collect()
}

/// 调用图片生成 API，将返回的 base64 图片解码并保存到 /tmp/。
async fn call_image_gen_api(
    client: &Client,
    prompt: &str,
    size: &str,
) -> Result<String, String> {
    let api_key = std::env::var("IMAGE_GEN_API_KEY")
        .map_err(|_| "IMAGE_GEN_API_KEY not set".to_string())?;
    let api_url = std::env::var("IMAGE_GEN_API_URL")
        .map_err(|_| "IMAGE_GEN_API_URL not set".to_string())?;
    let model = std::env::var("IMAGE_GEN_MODEL")
        .map_err(|_| "IMAGE_GEN_MODEL not set".to_string())?;

    let resp = client
        .post(&api_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&json!({
            "model": model,
            "prompt": prompt,
            "size": size,
            "return_base64": true
        }))
        .send()
        .await
        .map_err(|e| format!("request: {e}"))?;

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("parse: {e}"))?;

    let data = body
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .ok_or("no data in response")?;

    let b64 = data
        .get("b64_json")
        .and_then(|v| v.as_str())
        .ok_or("no b64_json in data")?;

    use base64::Engine;
    let img_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("base64 decode: {e}"))?;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let idx = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let path = format!("/tmp/claude_img_{}_{}.png", ts, idx);
    std::fs::write(&path, &img_bytes).map_err(|e| format!("write: {e}"))?;

    Ok(path)
}

/// 调用视频生成 API（异步：创建任务→轮询状态→下载视频→保存到 /tmp/）。
async fn call_video_gen_api(
    client: &Client,
    prompt: &str,
    seconds: &str,
    mode: &str,
    aspect_ratio: &str,
) -> Result<String, String> {
    let api_key = std::env::var("VIDEO_GEN_API_KEY")
        .map_err(|_| "VIDEO_GEN_API_KEY not set".to_string())?;
    let api_url = std::env::var("VIDEO_GEN_API_URL")
        .map_err(|_| "VIDEO_GEN_API_URL not set".to_string())?;
    let model = std::env::var("VIDEO_GEN_MODEL")
        .map_err(|_| "VIDEO_GEN_MODEL not set".to_string())?;
    let query_base = std::env::var("VIDEO_GEN_QUERY_URL")
        .unwrap_or_else(|_| "https://apihub.agnes-ai.com/agnesapi".to_string());

    let create_body = json!({
        "model": model,
        "prompt": prompt,
        "seconds": seconds,
        "mode": mode,
        "size": "720P",
        "aspect_ratio": aspect_ratio,
    });

    let create_resp = client
        .post(&api_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&create_body)
        .send()
        .await
        .map_err(|e| format!("video create request: {e}"))?;

    let create_status = create_resp.status();
    if !create_status.is_success() {
        let body = create_resp.text().await.unwrap_or_default();
        return Err(format!(
            "video create API returned {}: {}",
            create_status,
            &body[..body.len().min(200)]
        ));
    }

    let create_resp_body: Value = create_resp
        .json()
        .await
        .map_err(|e| format!("video create parse: {e}"))?;

    let video_id = create_resp_body
        .get("video_id")
        .and_then(|v| v.as_str())
        .or_else(|| create_resp_body.get("id").and_then(|v| v.as_str()))
        .ok_or_else(|| format!("No video_id in response: {}", create_resp_body))?;

    const MAX_ATTEMPTS: usize = 150;
    for attempt in 1..=MAX_ATTEMPTS {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let query_url = format!(
            "{}?video_id={}&model_name={}",
            query_base, video_id, model
        );

        let query_resp = match client
            .get(&query_url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };

        if !query_resp.status().is_success() {
            continue;
        }

        let query_body: Value = match query_resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let task_status = query_body
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if task_status == "completed" {
            let video_url = query_body
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("completed but no url in response")?;

            let video_resp = client
                .get(video_url)
                .send()
                .await
                .map_err(|e| format!("video download: {e}"))?;

            let video_bytes = video_resp
                .bytes()
                .await
                .map_err(|e| format!("video download read: {e}"))?;

            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            static V_COUNTER: std::sync::atomic::AtomicU32 =
                std::sync::atomic::AtomicU32::new(0);
            let idx = V_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let path = format!("/tmp/claude_video_{}_{}.mp4", ts, idx);
            std::fs::write(&path, &video_bytes)
                .map_err(|e| format!("video write: {e}"))?;

            return Ok(path);
        }

        if task_status == "failed" {
            let error = query_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Video generation failed: {error}"));
        }
    }

    Err(format!(
        "Video generation timed out after {} polling attempts",
        MAX_ATTEMPTS
    ))
}

/// 转发 Chat 请求到上游，如果响应包含 image_generation / video_generation
/// tool_calls，在适配器内部处理生成，将 tool_result 送回上游继续对话。
/// 返回最终的 Chat Completions 响应 JSON。
async fn run_chat_with_gen_loop(
    client: &Client,
    api_key: &str,
    headers: &HeaderMap,
    chat_url: &str,
    chat_req: &mut Value,
) -> Result<Value, String> {
    const MAX_ROUNDS: usize = 3;

    for round in 0..MAX_ROUNDS {
        let fwd_req = build_forwarded_request(client, api_key, headers, chat_url, chat_req);
        let resp = fwd_req.send().await.map_err(|e| format!("upstream: {e}"))?;
        let status = resp.status();
        let body_text = resp.text().await.map_err(|e| format!("read: {e}"))?;

        if !status.is_success() {
            return Err(format!("upstream error {}: {}", status, &body_text[..body_text.len().min(200)]));
        }
        let resp_body: Value =
            serde_json::from_str(&body_text).map_err(|e| format!("parse: {e}"))?;

        let img_tool_calls = extract_image_gen_tool_calls(&resp_body);
        let vid_tool_calls = extract_video_gen_tool_calls(&resp_body);
        if img_tool_calls.is_empty() && vid_tool_calls.is_empty() {
            return Ok(resp_body);
        }

        let mut tool_results = Vec::new();
        for (call_id, prompt, size) in &img_tool_calls {
            match call_image_gen_api(client, prompt, size).await {
                Ok(saved_path) => {
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": format!("Image generated and saved to {saved_path}")
                    }));
                }
                Err(e) => {
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": format!("Image generation failed: {e}")
                    }));
                }
            }
        }
        for (call_id, prompt, seconds, mode, aspect_ratio) in &vid_tool_calls {
            match call_video_gen_api(client, prompt, seconds, mode, aspect_ratio).await {
                Ok(saved_path) => {
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": format!("Video generated and saved to {saved_path}")
                    }));
                }
                Err(e) => {
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": format!("Video generation failed: {e}")
                    }));
                }
            }
        }

        let assistant_msg = resp_body
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("message"))
            .cloned()
            .unwrap_or(json!({}));
        if let Some(msgs) = chat_req.get_mut("messages").and_then(|v| v.as_array_mut()) {
            msgs.push(assistant_msg);
            for tr in tool_results {
                msgs.push(tr);
            }
        }
    }

    let fwd_req = build_forwarded_request(client, api_key, headers, chat_url, chat_req);
    let resp = fwd_req.send().await.map_err(|e| format!("upstream: {e}"))?;
    let status = resp.status();
    let body_text = resp.text().await.map_err(|e| format!("read: {e}"))?;
    if !status.is_success() {
        return Err(format!("upstream error {}: {}", status, &body_text[..body_text.len().min(200)]));
    }
    serde_json::from_str(&body_text).map_err(|e| format!("parse: {e}"))
}

/// 将 Chat Completions 响应 JSON 转为 Anthropic Messages 响应。
/// 非流式直接返回 JSON；流式拆成 SSE 事件。
fn convert_chat_to_anthropic_response(
    resp_body: &Value,
    is_stream: bool,
    req_body: &Value,
) -> Response {
    let anthropic_resp = anthropic_converter::chat_completions_to_anthropic(resp_body, req_body);

    if !is_stream {
        return (
            StatusCode::OK,
            [("Content-Type", "application/json")],
            anthropic_resp.to_string(),
        )
            .into_response();
    }

    let sse_stream = anthropic_converter::anthropic_response_to_sse_stream(anthropic_resp);
    let resp_headers = reqwest::header::HeaderMap::new();
    build_stream_response(StatusCode::OK, &resp_headers, sse_stream)
}

async fn handle_messages(
    State(state): State<Arc<AdapterState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {

    let req_body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")).into_response();
        }
    };

    let is_stream = req_body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut chat_req = anthropic_converter::anthropic_to_chat_completions(&req_body);
    if let Some(model) = chat_req.get("model").and_then(|v| v.as_str()) {
        chat_req["model"] = json!(anthropic_converter::map_model(model));
    }

    chat_req["stream"] = json!(false);

    if let Some(max_tokens) = chat_req.get("max_tokens").and_then(|v| v.as_u64()) {
        if max_tokens > 65536 {
            chat_req["max_tokens"] = json!(65536);
        }
    }

    inject_image_gen_tool(&mut chat_req);
    inject_video_gen_tool(&mut chat_req);

    let chat_url = format!("{}/chat/completions", state.real_base_url);

    let resp_body = match run_chat_with_gen_loop(
        &state.client,
        &state.api_key,
        &headers,
        &chat_url,
        &mut chat_req,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response();
        }
    };

    convert_chat_to_anthropic_response(&resp_body, is_stream, &req_body)
}


async fn proxy_response(
    resp: reqwest::Response,
    is_stream: bool,
    req_body: &Value,
) -> Response {
    let status = resp.status();
    let resp_headers = resp.headers().clone();

    if !is_stream {
        let resp_text = resp.text().await.unwrap_or_default();
        return (status, [("Content-Type", "application/json")], resp_text).into_response();
    }

    let byte_stream = resp.bytes_stream();
    let passthrough = converter::passthrough_sse_stream(byte_stream, req_body.clone());

    build_stream_response(status, &resp_headers, passthrough)
}

async fn proxy_chat_response(
    resp: reqwest::Response,
    is_stream: bool,
    req_body: &Value,
) -> Response {
    let status = resp.status();
    let resp_headers = resp.headers().clone();

    if !is_stream {
        let resp_body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("upstream parse error: {e}"))
                    .into_response();
            }
        };
        let converted = converter::chat_completions_to_responses(&resp_body, req_body);
        return (status, [("Content-Type", "application/json")], converted.to_string())
            .into_response();
    }

    let byte_stream = resp.bytes_stream();
    let sse_stream = converter::convert_sse_stream(byte_stream, req_body.clone());

    build_stream_response(status, &resp_headers, sse_stream)
}

fn build_stream_response(
    status: StatusCode,
    resp_headers: &reqwest::header::HeaderMap,
    stream: impl futures::Stream<Item = Result<String, std::io::Error>> + Send + 'static,
) -> Response {
    let mut resp_builder = Response::builder().status(status);
    for (key, value) in resp_headers.iter() {
        let k = key.as_str();
        if k == "content-length" || k == "content-encoding" || k == "transfer-encoding" {
            continue;
        }
        resp_builder = resp_builder.header(key, value.clone());
    }
    resp_builder = resp_builder.header("Content-Type?".strip_suffix('?').unwrap(), "text/event-stream");
    resp_builder = resp_builder.header("Cache-Control", "no-cache");
    resp_builder = resp_builder.header("Connection", "keep-alive");

    resp_builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "stream build error").into_response()
        })
}

#[cfg(test)]
mod multi_upstream_tests {
    use super::*;

    fn route(base: &str, models: &[&str]) -> UpstreamRoute {
        UpstreamRoute {
            base_url: base.to_string(),
            api_key: format!("key-{base}"),
            is_chat: AtomicBool::new(false),
            models: models.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn routes_by_model_name() {
        let routes = vec![route("agnes", &["agnes-3.0-flash"]), route("openai", &["gpt-4o"])];
        assert_eq!(resolve_multi_route(&routes, Some("agnes-3.0-flash")).base_url, "agnes");
        assert_eq!(resolve_multi_route(&routes, Some("gpt-4o")).base_url, "openai");
    }

    #[test]
    fn unknown_model_falls_back_to_first() {
        let routes = vec![route("agnes", &["agnes-3.0-flash"]), route("openai", &["gpt-4o"])];
        assert_eq!(resolve_multi_route(&routes, Some("unknown-model")).base_url, "agnes");
    }

    #[test]
    fn no_model_falls_back_to_first() {
        let routes = vec![route("agnes", &["agnes-3.0-flash"]), route("openai", &["gpt-4o"])];
        assert_eq!(resolve_multi_route(&routes, None).base_url, "agnes");
    }

    #[test]
    fn duplicate_model_first_wins() {
        let routes = vec![route("first", &["shared"]), route("second", &["shared"])];
        assert_eq!(resolve_multi_route(&routes, Some("shared")).base_url, "first");
    }
}
