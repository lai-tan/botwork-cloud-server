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

pub struct WebSearchHandler;

impl WebSearchHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default = "default_count")]
    count: u32,
    #[serde(default)]
    offset: u32,
}

fn default_count() -> u32 {
    10
}

fn create_web_search_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "query".to_string(),
        JsonSchema::string(Some("The search query.".to_string())),
    );
    properties.insert(
        "count".to_string(),
        JsonSchema::integer(Some(
            "Number of results to return (max 50). Default: 10.".to_string(),
        )),
    );
    properties.insert(
        "offset".to_string(),
        JsonSchema::integer(Some(
            "Number of results to skip. Default: 0.".to_string(),
        )),
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "web_search".to_string(),
        description: "Search the web using the configured search engine (Bing, Google, Brave, or Serper). Returns titles, URLs, and snippets for each result.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["query".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

struct SearchResult {
    results: Vec<Value>,
    total: u64,
}

async fn search_bing(args: &WebSearchArgs) -> Result<SearchResult, FunctionCallError> {
    let api_key = std::env::var("BING_SEARCH_API_KEY").map_err(|_| {
        FunctionCallError::RespondToModel(
            "BING_SEARCH_API_KEY environment variable not set.".to_string(),
        )
    })?;

    let count = args.count.min(50);
    let url = format!(
        "https://api.bing.microsoft.com/v7.0/search?q={}&count={}&offset={}",
        urlencoding::encode(&args.query),
        count,
        args.offset
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| FunctionCallError::Fatal(format!("failed to create HTTP client: {e}")))?;

    let response = client
        .get(&url)
        .header("Ocp-Apim-Subscription-Key", &api_key)
        .send()
        .await
        .map_err(|e| FunctionCallError::RespondToModel(format!("search request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(FunctionCallError::RespondToModel(format!(
            "Bing Search API returned {status}: {body}"
        )));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| FunctionCallError::RespondToModel(format!("failed to parse search response: {e}")))?;

    let results: Vec<Value> = body
        .get("webPages")
        .and_then(|w| w.get("value"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|item| {
                    serde_json::json!({
                        "title": item.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                        "url": item.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                        "snippet": item.get("snippet").and_then(|v| v.as_str()).unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let total = body
        .get("webPages")
        .and_then(|w| w.get("totalEstimatedMatches"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    Ok(SearchResult { results, total })
}

async fn search_google(args: &WebSearchArgs) -> Result<SearchResult, FunctionCallError> {
    let api_key = std::env::var("GOOGLE_SEARCH_API_KEY").map_err(|_| {
        FunctionCallError::RespondToModel(
            "GOOGLE_SEARCH_API_KEY environment variable not set.".to_string(),
        )
    })?;

    let engine_id = std::env::var("GOOGLE_SEARCH_ENGINE_ID").map_err(|_| {
        FunctionCallError::RespondToModel(
            "GOOGLE_SEARCH_ENGINE_ID environment variable not set.".to_string(),
        )
    })?;

    let count = args.count.min(10);
    let url = format!(
        "https://www.googleapis.com/customsearch/v1?q={}&num={}&start={}&key={}&cx={}",
        urlencoding::encode(&args.query),
        count,
        args.offset + 1,
        urlencoding::encode(&api_key),
        urlencoding::encode(&engine_id)
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| FunctionCallError::Fatal(format!("failed to create HTTP client: {e}")))?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| FunctionCallError::RespondToModel(format!("search request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(FunctionCallError::RespondToModel(format!(
            "Google Search API returned {status}: {body}"
        )));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| FunctionCallError::RespondToModel(format!("failed to parse search response: {e}")))?;

    let results: Vec<Value> = body
        .get("items")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|item| {
                    serde_json::json!({
                        "title": item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                        "url": item.get("link").and_then(|v| v.as_str()).unwrap_or(""),
                        "snippet": item.get("snippet").and_then(|v| v.as_str()).unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let total = body
        .get("searchInformation")
        .and_then(|s| s.get("totalResults"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    Ok(SearchResult { results, total })
}

async fn search_brave(args: &WebSearchArgs) -> Result<SearchResult, FunctionCallError> {
    let api_key = std::env::var("BRAVE_SEARCH_API_KEY").map_err(|_| {
        FunctionCallError::RespondToModel(
            "BRAVE_SEARCH_API_KEY environment variable not set.".to_string(),
        )
    })?;

    let count = args.count.min(50);
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}&offset={}",
        urlencoding::encode(&args.query),
        count,
        args.offset
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| FunctionCallError::Fatal(format!("failed to create HTTP client: {e}")))?;

    let response = client
        .get(&url)
        .header("X-Subscription-Token", &api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| FunctionCallError::RespondToModel(format!("search request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(FunctionCallError::RespondToModel(format!(
            "Brave Search API returned {status}: {body}"
        )));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| FunctionCallError::RespondToModel(format!("failed to parse search response: {e}")))?;

    let results: Vec<Value> = body
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|item| {
                    serde_json::json!({
                        "title": item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                        "url": item.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                        "snippet": item.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let total = body
        .get("web")
        .and_then(|w| w.get("count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    Ok(SearchResult { results, total })
}

async fn search_serper(args: &WebSearchArgs) -> Result<SearchResult, FunctionCallError> {
    let api_key = std::env::var("SERPER_API_KEY").map_err(|_| {
        FunctionCallError::RespondToModel(
            "SERPER_API_KEY environment variable not set.".to_string(),
        )
    })?;

    let count = args.count.min(50);
    let body = serde_json::json!({
        "q": args.query,
        "num": count,
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| FunctionCallError::Fatal(format!("failed to create HTTP client: {e}")))?;

    let response = client
        .post("https://google.serper.dev/search")
        .header("X-API-KEY", &api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| FunctionCallError::RespondToModel(format!("search request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(FunctionCallError::RespondToModel(format!(
            "Serper API returned {status}: {body}"
        )));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| FunctionCallError::RespondToModel(format!("failed to parse search response: {e}")))?;

    let results: Vec<Value> = body
        .get("organic")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|item| {
                    serde_json::json!({
                        "title": item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                        "url": item.get("link").and_then(|v| v.as_str()).unwrap_or(""),
                        "snippet": item.get("snippet").and_then(|v| v.as_str()).unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let total = results.len() as u64;

    Ok(SearchResult { results, total })
}

impl ToolExecutor<ToolInvocation> for WebSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("web_search")
    }

    fn spec(&self) -> ToolSpec {
        create_web_search_tool()
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
                        "web_search handler received unsupported payload".to_string(),
                    ));
                }
            };

            let args: WebSearchArgs = parse_arguments(&arguments)?;

            let provider =
                std::env::var("WEB_SEARCH_PROVIDER").unwrap_or_else(|_| "bing".to_string());

            let search_result = match provider.as_str() {
                "bing" => search_bing(&args).await?,
                "google" => search_google(&args).await?,
                "brave" => search_brave(&args).await?,
                "serper" => search_serper(&args).await?,
                _ => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "Unknown WEB_SEARCH_PROVIDER: {provider}. Supported: bing, google, brave, serper"
                    )));
                }
            };

            let result = serde_json::json!({
                "results": search_result.results,
                "total": search_result.total,
            });

            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                result.to_string(),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for WebSearchHandler {}
