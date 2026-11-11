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

pub struct ImageGenerationHandler;

impl ImageGenerationHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct ImageGenArgs {
    prompt: String,
    #[serde(default = "default_size")]
    size: String,
    #[serde(default = "default_n")]
    n: u32,
}

fn default_size() -> String {
    "1024x1024".to_string()
}

fn default_n() -> u32 {
    1
}

fn create_image_generation_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "prompt".to_string(),
        JsonSchema::string(Some("Description of the image to generate.".to_string())),
    );
    properties.insert(
        "size".to_string(),
        JsonSchema::string(Some(
            "Image size: 1024x1024, 512x512, or 256x256. Default: 1024x1024.".to_string(),
        )),
    );
    properties.insert(
        "n".to_string(),
        JsonSchema::integer(Some(
            "Number of images to generate (1-4). Default: 1.".to_string(),
        )),
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "image_generation".to_string(),
        description: "Generate images from text descriptions using an image generation API. Supports OpenAI DALL-E, Azure OpenAI, and Agnes Image (set IMAGE_GEN_MODEL env var, e.g. agnes-image-2.5-flash).".to_string(),
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

impl ToolExecutor<ToolInvocation> for ImageGenerationHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("image_generation")
    }

    fn spec(&self) -> ToolSpec {
        create_image_generation_tool()
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
                        "image_generation handler received unsupported payload".to_string(),
                    ));
                }
            };

            let args: ImageGenArgs = parse_arguments(&arguments)?;

            let api_key = std::env::var("IMAGE_GEN_API_KEY")
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .or_else(|_| std::env::var("CODEX_API_KEY"))
                .map_err(|_| {
                    FunctionCallError::RespondToModel(
                        "No API key found. Set IMAGE_GEN_API_KEY, OPENAI_API_KEY, or CODEX_API_KEY."
                            .to_string(),
                    )
                })?;

            let api_url = std::env::var("IMAGE_GEN_API_URL").unwrap_or_else(|_| {
                "https://api.openai.com/v1/images/generations".to_string()
            });

            let image_gen_model = std::env::var("IMAGE_GEN_MODEL").ok();
            let n = args.n.clamp(1, 4);

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .map_err(|e| {
                    FunctionCallError::Fatal(format!("failed to create HTTP client: {e}"))
                })?;

            let body = if let Some(model) = &image_gen_model {
                serde_json::json!({
                    "model": model,
                    "prompt": args.prompt,
                    "size": args.size,
                    "return_base64": true,
                })
            } else {
                serde_json::json!({
                    "prompt": args.prompt,
                    "size": args.size,
                    "n": n,
                    "response_format": "b64_json",
                })
            };

            let response = client
                .post(&api_url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    FunctionCallError::RespondToModel(format!(
                        "image generation request failed: {e}"
                    ))
                })?;

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(FunctionCallError::RespondToModel(format!(
                    "Image generation API returned {}: {}",
                    status, body
                )));
            }

            let resp_body: Value = response.json().await.map_err(|e| {
                FunctionCallError::RespondToModel(format!("failed to parse response: {e}"))
            })?;

            let images: Vec<String> = resp_body
                .get("data")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            item.get("b64_json")
                                .and_then(|v| v.as_str())
                                .map(|s| format!("data:image/png;base64,{s}"))
                                .or_else(|| {
                                    item.get("url")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string())
                                })
                        })
                        .collect()
                })
                .unwrap_or_default();

            let result = serde_json::json!({
                "images": images,
                "count": images.len(),
            });

            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                result.to_string(),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for ImageGenerationHandler {}
