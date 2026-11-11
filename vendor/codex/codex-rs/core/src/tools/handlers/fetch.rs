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

pub struct FetchHandler;

impl FetchHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
    #[serde(default = "default_method")]
    method: String,
    #[serde(default)]
    headers: Option<Value>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default = "default_max_size")]
    max_size_bytes: usize,
}

fn default_method() -> String {
    "GET".to_string()
}

fn default_timeout() -> u64 {
    30_000
}

fn default_max_size() -> usize {
    1_048_576
}

fn create_fetch_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "url".to_string(),
        JsonSchema::string(Some("The URL to fetch.".to_string())),
    );
    properties.insert(
        "method".to_string(),
        JsonSchema::string(Some(
            "HTTP method (GET, POST, PUT, DELETE, etc.). Default: GET.".to_string(),
        )),
    );
    properties.insert(
        "headers".to_string(),
        JsonSchema::object(BTreeMap::new(), None, None),
    );
    properties.insert(
        "body".to_string(),
        JsonSchema::string(Some("Request body for POST/PUT requests.".to_string())),
    );
    properties.insert(
        "timeout_ms".to_string(),
        JsonSchema::integer(Some(
            "Request timeout in milliseconds. Default: 30000.".to_string(),
        )),
    );
    properties.insert(
        "max_size_bytes".to_string(),
        JsonSchema::integer(Some(
            "Maximum response body size in bytes. Default: 1048576 (1MB).".to_string(),
        )),
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "fetch".to_string(),
        description: "Fetch content from a URL using HTTP. Returns status code, headers, and body. Useful for web scraping and API calls.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["url".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

impl ToolExecutor<ToolInvocation> for FetchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("fetch")
    }

    fn spec(&self) -> ToolSpec {
        create_fetch_tool()
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
                        "fetch handler received unsupported payload".to_string(),
                    ));
                }
            };

            let args: FetchArgs = parse_arguments(&arguments)?;

            if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
                return Err(FunctionCallError::RespondToModel(
                    "URL must start with http:// or https://".to_string(),
                ));
            }

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(args.timeout_ms))
                .build()
                .map_err(|e| {
                    FunctionCallError::Fatal(format!("failed to create HTTP client: {e}"))
                })?;

            let method = match args.method.to_uppercase().as_str() {
                "GET" => reqwest::Method::GET,
                "POST" => reqwest::Method::POST,
                "PUT" => reqwest::Method::PUT,
                "DELETE" => reqwest::Method::DELETE,
                "PATCH" => reqwest::Method::PATCH,
                "HEAD" => reqwest::Method::HEAD,
                _ => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "unsupported HTTP method: {}",
                        args.method
                    )));
                }
            };

            let mut request = client.request(method, &args.url);

            if let Some(headers) = &args.headers {
                if let Some(obj) = headers.as_object() {
                    for (key, val) in obj {
                        if let Some(s) = val.as_str() {
                            request = request.header(key, s);
                        }
                    }
                }
            }

            if let Some(body) = &args.body {
                request = request.body(body.clone());
            }

            let response = request.send().await.map_err(|e| {
                FunctionCallError::RespondToModel(format!("request failed: {e}"))
            })?;

            let status = response.status().as_u16();
            let resp_headers: BTreeMap<String, String> = response
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            let content_type = resp_headers
                .get("content-type")
                .cloned()
                .unwrap_or_default();

            let body_bytes = response.bytes().await.map_err(|e| {
                FunctionCallError::RespondToModel(format!("failed to read body: {e}"))
            })?;

            let truncated = body_bytes.len() > args.max_size_bytes;
            let body_slice = if truncated {
                &body_bytes[..args.max_size_bytes]
            } else {
                &body_bytes[..]
            };

            let body_text = if content_type.contains("json") {
                String::from_utf8_lossy(body_slice).to_string()
            } else if content_type.contains("text")
                || content_type.contains("html")
                || content_type.contains("xml")
            {
                String::from_utf8_lossy(body_slice).to_string()
            } else {
                format!("<binary data, {} bytes>", body_bytes.len())
            };

            let result = serde_json::json!({
                "status": status,
                "headers": resp_headers,
                "body": body_text,
                "truncated": truncated,
                "content_type": content_type,
            });

            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                result.to_string(),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for FetchHandler {}
