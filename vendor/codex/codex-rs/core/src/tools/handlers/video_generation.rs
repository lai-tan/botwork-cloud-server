use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

pub struct VideoGenerationHandler;

impl VideoGenerationHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct VideoGenArgs {
    prompt: String,
    #[serde(default = "default_seconds")]
    seconds: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default = "default_aspect_ratio")]
    aspect_ratio: String,
}

fn default_seconds() -> String {
    "5".to_string()
}

fn default_mode() -> String {
    "text".to_string()
}

fn default_aspect_ratio() -> String {
    "16:9".to_string()
}

fn create_video_generation_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "prompt".to_string(),
        JsonSchema::string(Some("Description of the video to generate.".to_string())),
    );
    properties.insert(
        "seconds".to_string(),
        JsonSchema::string(Some(
            "Video duration: \"4\" to \"12\" seconds. Default: \"5\".".to_string(),
        )),
    );
    properties.insert(
        "mode".to_string(),
        JsonSchema::string(Some(
            "Generation mode: \"text\", \"keyframe\", or \"reference\". Default: \"text\".".to_string(),
        )),
    );
    properties.insert(
        "aspect_ratio".to_string(),
        JsonSchema::string(Some(
            "Aspect ratio: 16:9, 9:16, 1:1, 4:3, 3:4, 21:9. Default: 16:9.".to_string(),
        )),
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "video_generation".to_string(),
        description: "Generate videos from text descriptions using Agnes Video API. Supports agnes-video-2.5-flash (720P, 4-12 seconds). Set VIDEO_GEN_API_KEY and VIDEO_GEN_MODEL env vars.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["prompt".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

impl ToolExecutor<ToolInvocation> for VideoGenerationHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("video_generation")
    }

    fn spec(&self) -> ToolSpec {
        create_video_generation_tool()
    }

    fn handle<'a>(
        &'a self,
        invocation: ToolInvocation,
    ) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation { payload, .. } = invocation;

            let arguments = match payload {
                ToolPayload::Function { arguments } => arguments,
                _ => {
                    return Err(FunctionCallError::RespondToModel(
                        "video_generation handler received unsupported payload".to_string(),
                    ));
                }
            };

            let args: VideoGenArgs = parse_arguments(&arguments)?;

            let api_key = std::env::var("VIDEO_GEN_API_KEY")
                .or_else(|_| std::env::var("IMAGE_GEN_API_KEY"))
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .or_else(|_| std::env::var("CODEX_API_KEY"))
                .map_err(|_| {
                    FunctionCallError::RespondToModel(
                        "No API key found. Set VIDEO_GEN_API_KEY, IMAGE_GEN_API_KEY, OPENAI_API_KEY, or CODEX_API_KEY."
                            .to_string(),
                    )
                })?;

            let api_url = std::env::var("VIDEO_GEN_API_URL").unwrap_or_else(|_| {
                "https://apihub.agnes-ai.com/v1/videos".to_string()
            });

            let model = std::env::var("VIDEO_GEN_MODEL").unwrap_or_else(|_| {
                "agnes-video-2.5-flash".to_string()
            });

            let query_base = std::env::var("VIDEO_GEN_QUERY_URL").unwrap_or_else(|_| {
                "https://apihub.agnes-ai.com/agnesapi".to_string()
            });

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .map_err(|e| {
                    FunctionCallError::Fatal(format!("failed to create HTTP client: {e}"))
                })?;

            let body = serde_json::json!({
                "model": model,
                "prompt": args.prompt,
                "seconds": args.seconds,
                "mode": args.mode,
                "size": "720P",
                "aspect_ratio": args.aspect_ratio,
            });

            tracing::info!("video_generation: POST create video, model={model}, prompt={}", args.prompt);

            let response = client
                .post(&api_url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    FunctionCallError::RespondToModel(format!(
                        "video generation request failed: {e}"
                    ))
                })?;

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                tracing::warn!("video_generation: POST failed, status={status}, body={body}");
                return Err(FunctionCallError::RespondToModel(format!(
                    "Video generation API returned {}: {}",
                    status, body
                )));
            }

            let resp_body: Value = response.json().await.map_err(|e| {
                FunctionCallError::RespondToModel(format!("failed to parse response: {e}"))
            })?;

            let video_id = resp_body
                .get("video_id")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    resp_body
                        .get("id")
                        .and_then(|v| v.as_str())
                })
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(format!(
                        "No video_id in response: {}",
                        resp_body
                    ))
                })?;

            tracing::info!("video_generation: video created, video_id={video_id}, polling for status...");

            let max_attempts = 150;
            for attempt in 1..=max_attempts {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                let query_url = format!(
                    "{}?video_id={}&model_name={}",
                    query_base, video_id, model
                );

                let query_resp = client
                    .get(&query_url)
                    .header("Authorization", format!("Bearer {api_key}"))
                    .send()
                    .await;

                let query_resp = match query_resp {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("video_generation: poll attempt {attempt}/{max_attempts} failed: {e}");
                        continue;
                    }
                };

                if !query_resp.status().is_success() {
                    tracing::debug!("video_generation: poll attempt {attempt}/{max_attempts}, HTTP {}", query_resp.status());
                    continue;
                }

                let query_body: Value = query_resp.json().await.map_err(|e| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to parse query response: {e}"
                    ))
                })?;

                let task_status = query_body
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                let progress = query_body
                    .get("progress")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                tracing::info!("video_generation: poll attempt {attempt}/{max_attempts}, status={task_status}, progress={progress}");

                if task_status == "completed" {
                    let video_url = query_body
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    tracing::info!("video_generation: completed! video_id={video_id}, url={video_url}");

                    let result = serde_json::json!({
                        "videos": [video_url],
                        "video_id": video_id,
                        "status": "completed",
                        "count": 1,
                    });

                    return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        result.to_string(),
                        Some(true),
                    )));
                }

                if task_status == "failed" {
                    let error = query_body
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(FunctionCallError::RespondToModel(format!(
                        "Video generation failed: {error}"
                    )));
                }
            }

            tracing::warn!("video_generation: timed out after {max_attempts} polling attempts ({}s)", max_attempts * 2);
            Err(FunctionCallError::RespondToModel(format!(
                "Video generation timed out after {} polling attempts",
                max_attempts
            )))
        })
    }
}

impl CoreToolRuntime for VideoGenerationHandler {}