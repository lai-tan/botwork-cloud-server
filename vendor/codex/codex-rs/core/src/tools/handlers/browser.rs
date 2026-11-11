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

pub struct BrowserHandler;

impl BrowserHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct BrowserArgs {
    action: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30_000
}

fn create_browser_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "action".to_string(),
        JsonSchema::string(Some(
            "Browser action: navigate (go to url, return page text), screenshot (go to url, return screenshot), click (go to url, click selector), type (go to url, type text into selector), evaluate (run JS on current page if no url, or navigate first if url given), get_text (go to url, get selector text), login (go to url, fill username/password, click submit, return page text), close (close the browser session). The browser PERSISTS across calls — cookies and login state are preserved."
                .to_string(),
        )),
    );
    properties.insert(
        "url".to_string(),
        JsonSchema::string(Some(
            "URL to navigate to. Required for most actions.".to_string(),
        )),
    );
    properties.insert(
        "selector".to_string(),
        JsonSchema::string(Some(
            "CSS selector for click/type/get_text actions.".to_string(),
        )),
    );
    properties.insert(
        "text".to_string(),
        JsonSchema::string(Some("Text to type (for type action).".to_string())),
    );
    properties.insert(
        "script".to_string(),
        JsonSchema::string(Some(
            "JavaScript to evaluate (for evaluate action).".to_string(),
        )),
    );
    properties.insert(
        "username".to_string(),
        JsonSchema::string(Some(
            "Username for login action.".to_string(),
        )),
    );
    properties.insert(
        "password".to_string(),
        JsonSchema::string(Some(
            "Password for login action.".to_string(),
        )),
    );
    properties.insert(
        "timeout_ms".to_string(),
        JsonSchema::integer(Some(
            "Action timeout in milliseconds. Default: 30000.".to_string(),
        )),
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "browser".to_string(),
        description: "Control a Chrome/Edge browser with PERSISTENT session. The browser stays open across calls — cookies, login state, and page context are preserved. Use 'navigate' to go to a URL, 'evaluate' (without url) to interact with the current page, 'click'/'type'/'get_text' for element interaction, 'login' for form-based login, 'screenshot' for visual capture, and 'close' to close the browser when done.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["action".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

impl ToolExecutor<ToolInvocation> for BrowserHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("browser")
    }

    fn spec(&self) -> ToolSpec {
        create_browser_tool()
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
                        "browser handler received unsupported payload".to_string(),
                    ));
                }
            };

            let args: BrowserArgs = parse_arguments(&arguments)?;

            const CDP_PORT: u16 = 9222;
            let user_data_dir = std::env::temp_dir().join("chrome-cdp-persistent");
            let user_data_dir_arg = format!("--user-data-dir={}", user_data_dir.to_str().unwrap_or(""));

            let timeout = std::time::Duration::from_millis(args.timeout_ms);
            let deadline = tokio::time::Instant::now() + timeout;

            if args.action == "close" {
                let quick_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
                if let Ok(Ok(ws_url)) = tokio::time::timeout_at(
                    quick_deadline,
                    wait_for_cdp(CDP_PORT, quick_deadline),
                ).await {
                    let _ = cdp_send(&ws_url, 1, "Browser.close", serde_json::json!({})).await;
                }
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    serde_json::json!({"action": "close", "result": "browser closed"})
                        .to_string(),
                    Some(true),
                )));
            }

            let ws_url = match tokio::time::timeout_at(
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                wait_for_cdp(CDP_PORT, tokio::time::Instant::now() + std::time::Duration::from_secs(2)),
            ).await {
                Ok(Ok(url)) => {
                    tracing::info!("browser tool: reusing existing browser on port {}", CDP_PORT);
                    url
                }
                _ => {
                    tracing::info!("browser tool: no existing browser, starting new one on port {}", CDP_PORT);
                    let chrome_path = find_browser().ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "No suitable browser found. The browser tool requires Chrome or Edge.\n\
                             To fix this:\n\
                             1. Install Google Chrome from https://www.google.com/chrome/\n\
                             2. Or set CHROME_PATH environment variable to the browser executable path\n\
                             3. On Windows, Microsoft Edge (pre-installed) is also supported automatically".to_string(),
                        )
                    })?;

                    let port_arg = format!("--remote-debugging-port={CDP_PORT}");
                    let mut args = vec![
                        "--disable-gpu".to_string(),
                        "--no-first-run".to_string(),
                        "--no-default-browser-check".to_string(),
                        "--new-window".to_string(),
                        "--start-maximized".to_string(),
                        "--remote-allow-origins=*".to_string(),
                        port_arg,
                        user_data_dir_arg,
                    ];
                    #[cfg(target_os = "linux")]
                    args.insert(2, "--no-sandbox".to_string());
                    let _chrome_proc = std::process::Command::new(&chrome_path)
                        .args(&args)
                        .env_remove("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS")
                        .env_remove("WEBVIEW2_USER_DATA_FOLDER")
                        .env_remove("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .map_err(|e| {
                            FunctionCallError::Fatal(format!("failed to start browser: {e}"))
                        })?;

                    match tokio::time::timeout_at(
                        deadline,
                        wait_for_cdp(CDP_PORT, deadline),
                    ).await {
                        Ok(Ok(url)) => url,
                        Ok(Err(e)) => return Err(e),
                        Err(_) => return Err(FunctionCallError::Fatal(format!(
                            "timeout waiting for browser CDP on port {CDP_PORT}"
                        ))),
                    }
                }
            };

            let result = match args.action.as_str() {
                "navigate" => {
                    let url = args.url.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "url is required for navigate".to_string(),
                        )
                    })?;
                    match tokio::time::timeout_at(
                        deadline,
                        do_navigate_and_get_text(&ws_url, &url),
                    )
                    .await
                    {
                        Ok(r) => r?,
                        Err(_) => return Err(FunctionCallError::Fatal(
                            "navigate timeout".to_string(),
                        )),
                    }
                }
                "screenshot" => {
                    let url = args.url.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "url is required for screenshot".to_string(),
                        )
                    })?;
                    match tokio::time::timeout_at(
                        deadline,
                        do_screenshot(&ws_url, &url),
                    )
                    .await
                    {
                        Ok(r) => r?,
                        Err(_) => return Err(FunctionCallError::Fatal(
                            "screenshot timeout".to_string(),
                        )),
                    }
                }
                "click" => {
                    let url = args.url.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "url is required for click".to_string(),
                        )
                    })?;
                    let selector = args.selector.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "selector is required for click".to_string(),
                        )
                    })?;
                    match tokio::time::timeout_at(
                        deadline,
                        do_click(&ws_url, &url, &selector),
                    )
                    .await
                    {
                        Ok(r) => r?,
                        Err(_) => return Err(FunctionCallError::Fatal(
                            "click timeout".to_string(),
                        )),
                    }
                }
                "type" => {
                    let url = args.url.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "url is required for type".to_string(),
                        )
                    })?;
                    let selector = args.selector.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "selector is required for type".to_string(),
                        )
                    })?;
                    let text = args.text.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "text is required for type".to_string(),
                        )
                    })?;
                    match tokio::time::timeout_at(
                        deadline,
                        do_type_text(&ws_url, &url, &selector, &text),
                    )
                    .await
                    {
                        Ok(r) => r?,
                        Err(_) => return Err(FunctionCallError::Fatal(
                            "type timeout".to_string(),
                        )),
                    }
                }
                "evaluate" => {
                    let script = args.script.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "script is required for evaluate".to_string(),
                        )
                    })?;
                    match tokio::time::timeout_at(
                        deadline,
                        do_evaluate(&ws_url, args.url.as_deref(), &script),
                    )
                    .await
                    {
                        Ok(r) => r?,
                        Err(_) => return Err(FunctionCallError::Fatal(
                            "evaluate timeout".to_string(),
                        )),
                    }
                }
                "get_text" => {
                    let url = args.url.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "url is required for get_text".to_string(),
                        )
                    })?;
                    let selector = args.selector.unwrap_or_else(|| "body".to_string());
                    match tokio::time::timeout_at(
                        deadline,
                        do_get_text(&ws_url, &url, &selector),
                    )
                    .await
                    {
                        Ok(r) => r?,
                        Err(_) => return Err(FunctionCallError::Fatal(
                            "get_text timeout".to_string(),
                        )),
                    }
                }
                "login" => {
                    let url = args.url.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "url is required for login".to_string(),
                        )
                    })?;
                    let username = args.username.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "username is required for login".to_string(),
                        )
                    })?;
                    let password = args.password.ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "password is required for login".to_string(),
                        )
                    })?;
                    match tokio::time::timeout_at(
                        deadline,
                        do_login(&ws_url, &url, &username, &password),
                    )
                    .await
                    {
                        Ok(r) => r?,
                        Err(_) => return Err(FunctionCallError::Fatal(
                            "login timeout".to_string(),
                        )),
                    }
                }
                _ => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "unknown browser action: {}",
                        args.action
                    )));
                }
            };

            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                result.to_string(),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for BrowserHandler {}

fn find_browser() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("CHROME_PATH") {
        let p = std::path::PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(path) = std::env::var("EDGE_PATH") {
        let p = std::path::PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    let candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium-browser",
        "/usr/bin/chromium",
        "/usr/bin/microsoft-edge",
        "/usr/bin/microsoft-edge-stable",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];
    for c in &candidates {
        let p = std::path::PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn find_free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(9222)
}

async fn wait_for_cdp(
    port: u16,
    deadline: tokio::time::Instant,
) -> Result<String, FunctionCallError> {
    let mut attempt = 0u32;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(FunctionCallError::Fatal(format!(
                "timeout waiting for Chrome CDP on port {port}"
            )));
        }
        let url = format!("http://127.0.0.1:{port}/json");
        attempt += 1;
        if attempt <= 3 || attempt % 10 == 0 {
            tracing::debug!("browser tool: CDP poll attempt {} on {}", attempt, url);

        }
        match reqwest::get(&url).await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(text) = resp.text().await {
                    if let Ok(pages) = serde_json::from_str::<Vec<Value>>(&text) {
                        for page in pages {
                            if page
                                .get("type")
                                .and_then(|t| t.as_str())
                                .map(|s| s == "page")
                                .unwrap_or(false)
                            {
                                if let Some(ws) = page
                                    .get("webSocketDebuggerUrl")
                                    .and_then(|u| u.as_str())
                                {
                                    tracing::info!("browser tool: CDP ready on port {}", port);
                                    return Ok(ws.to_string());
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn cdp_send(
    ws_url: &str,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, FunctionCallError> {
    use futures_util::SinkExt;
    use futures_util::StreamExt;
    use tokio_tungstenite::connect_async;

    let (mut ws, _) = connect_async(ws_url).await.map_err(|e| {
        FunctionCallError::Fatal(format!("CDP connect failed: {e}"))
    })?;

    let msg = serde_json::json!({
        "id": id,
        "method": method,
        "params": params,
    });
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        msg.to_string().into(),
    ))
    .await
    .map_err(|e| FunctionCallError::Fatal(format!("CDP send failed: {e}")))?;

    while let Some(resp) = ws.next().await {
        let resp = resp.map_err(|e| {
            FunctionCallError::Fatal(format!("CDP recv failed: {e}"))
        })?;
        if let tokio_tungstenite::tungstenite::Message::Text(text) = resp {
            let v: Value = serde_json::from_str(&text).map_err(|e| {
                FunctionCallError::Fatal(format!("CDP parse failed: {e}"))
            })?;
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return Ok(v);
            }
        }
    }
    Err(FunctionCallError::Fatal(
        "CDP connection closed".to_string(),
    ))
}

async fn cdp_navigate(ws_url: &str, url: &str) -> Result<(), FunctionCallError> {
    let _ = cdp_send(ws_url, 1, "Page.enable", serde_json::json!({})).await;
    let resp = cdp_send(ws_url, 2, "Page.navigate", serde_json::json!({"url": url})).await?;
    if let Some(err) = resp.get("result").and_then(|r| r.get("errorText")).and_then(|e| e.as_str()) {
        return Err(FunctionCallError::RespondToModel(format!(
            "navigation error: {err}"
        )));
    }
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    Ok(())
}

async fn cdp_eval(ws_url: &str, id: u64, script: &str) -> Result<Value, FunctionCallError> {
    let resp = cdp_send(ws_url, id, "Runtime.evaluate", serde_json::json!({"expression": script, "returnByValue": true, "awaitPromise": true})).await?;
    Ok(resp.get("result").and_then(|r| r.get("result")).cloned().unwrap_or(Value::Null))
}

async fn do_navigate_and_get_text(ws_url: &str, url: &str) -> Result<Value, FunctionCallError> {
    cdp_navigate(ws_url, url).await?;
    let script = "document.body ? document.body.innerText : ''";
    let val = cdp_eval(ws_url, 3, script).await?;
    let text = val.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let title_script = "document.title || ''";
    let title_val = cdp_eval(ws_url, 4, title_script).await?;
    let title = title_val.get("value").and_then(|v| v.as_str()).unwrap_or("");
    Ok(serde_json::json!({"action": "navigate", "url": url, "title": title, "text": text}))
}

async fn do_screenshot(ws_url: &str, url: &str) -> Result<Value, FunctionCallError> {
    cdp_navigate(ws_url, url).await?;
    let resp = cdp_send(ws_url, 3, "Page.captureScreenshot", serde_json::json!({"format": "png"})).await?;
    let data = resp.get("result").and_then(|r| r.get("data")).and_then(|d| d.as_str()).unwrap_or("");
    Ok(serde_json::json!({"action": "screenshot", "url": url, "data": data}))
}

async fn do_click(ws_url: &str, url: &str, selector: &str) -> Result<Value, FunctionCallError> {
    cdp_navigate(ws_url, url).await?;
    let script = format!(
        "(function(){{var el=document.querySelector({selector:?});if(!el)return 'element not found';el.click();return 'clicked';}})()",
        selector = selector
    );
    let val = cdp_eval(ws_url, 3, &script).await?;
    let result = val.get("value").and_then(|v| v.as_str()).unwrap_or("");
    Ok(serde_json::json!({"action": "click", "url": url, "selector": selector, "result": result}))
}

async fn do_type_text(
    ws_url: &str,
    url: &str,
    selector: &str,
    text: &str,
) -> Result<Value, FunctionCallError> {
    cdp_navigate(ws_url, url).await?;
    let script = format!(
        "(function(){{var el=document.querySelector({selector:?});if(!el)return 'element not found';el.value={text:?};el.dispatchEvent(new Event('input',{{bubbles:true}}));return 'typed';}})()",
        selector = selector,
        text = text
    );
    let val = cdp_eval(ws_url, 3, &script).await?;
    let result = val.get("value").and_then(|v| v.as_str()).unwrap_or("");
    Ok(serde_json::json!({"action": "type", "url": url, "selector": selector, "text": text, "result": result}))
}

async fn do_evaluate(
    ws_url: &str,
    url: Option<&str>,
    script: &str,
) -> Result<Value, FunctionCallError> {
    if let Some(url) = url {
        cdp_navigate(ws_url, url).await?;
    }
    let val = cdp_eval(ws_url, 3, script).await?;
    Ok(serde_json::json!({"action": "evaluate", "result": val}))
}

async fn do_get_text(
    ws_url: &str,
    url: &str,
    selector: &str,
) -> Result<Value, FunctionCallError> {
    cdp_navigate(ws_url, url).await?;
    let script = format!(
        "(document.querySelector({selector:?})?.textContent) || ''",
        selector = selector
    );
    let val = cdp_eval(ws_url, 3, &script).await?;
    let text = val.get("value").and_then(|v| v.as_str()).unwrap_or("");
    Ok(serde_json::json!({"action": "get_text", "url": url, "selector": selector, "text": text}))
}
async fn do_login(
    ws_url: &str,
    url: &str,
    username: &str,
    password: &str,
) -> Result<Value, FunctionCallError> {
    cdp_navigate(ws_url, url).await?;

    let wait_script = r#"(async()=>{
        for(let i=0;i<30;i++){
            if(document.querySelector("input[type=text],input[type=email],input[name=username],input[type=password]"))break;
            await new Promise(r=>setTimeout(r,500));
        }
        return "ready";
    })()"#;
    let _ = cdp_eval(ws_url, 3, wait_script).await?;

    let diag_script = r#"(function(){
        var u=document.querySelector("input[type=text],input[type=email],input[name=username]");
        var p=document.querySelector("input[type=password]");
        var b=document.querySelector("button[type=submit],button");
        return JSON.stringify({
            hasUser: !!u, userType: u?u.type:null, userName: u?u.name:null,
            hasPass: !!p, passType: p?p.type:null, passName: p?p.name:null,
            hasBtn: !!b, btnType: b?b.type:null, btnText: b?b.textContent.slice(0,50):null
        });
    })()"#;
    let diag_val = cdp_eval(ws_url, 4, diag_script).await?;
    let diag = diag_val.get("value").and_then(|v| v.as_str()).unwrap_or("");

    let set_user_script = format!(
        r#"(function(){{
            var u=document.querySelector("input[type=text],input[type=email],input[name=username]");
            if(!u)return "no_username_input";
            var s=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,"value").set;
            u.focus();
            s.call(u,{username:?});
            u.dispatchEvent(new Event("input",{{bubbles:true}}));
            u.dispatchEvent(new Event("change",{{bubbles:true}}));
            return u.value;
        }})()"#,
        username = username,
    );
    let user_result = cdp_eval(ws_url, 5, &set_user_script).await?;
    let user_val = user_result.get("value").and_then(|v| v.as_str()).unwrap_or("");
    tracing::info!("browser tool: login username set to '{}'", user_val);
    if user_val.is_empty() || user_val == "no_username_input" {
        return Ok(serde_json::json!({
            "action": "login", "url": url, "error": user_val, "diag": diag
        }));
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let set_pw_script = format!(
        r#"(function(){{
            var p=document.querySelector("input[type=password]");
            if(!p)return "no_password_input";
            var s=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,"value").set;
            p.focus();
            s.call(p,{password:?});
            p.dispatchEvent(new Event("input",{{bubbles:true}}));
            p.dispatchEvent(new Event("change",{{bubbles:true}}));
            return p.value.length;
        }})()"#,
        password = password,
    );
    let pw_result = cdp_eval(ws_url, 6, &set_pw_script).await?;
    let pw_len = pw_result.get("value").and_then(|v| v.as_i64()).unwrap_or(-1);
    tracing::info!("browser tool: login password set, length = {}", pw_len);
    if pw_len < 0 {
        let err = pw_result.get("value").and_then(|v| v.as_str()).unwrap_or("error");
        return Ok(serde_json::json!({
            "action": "login", "url": url, "error": err, "diag": diag
        }));
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let click_script = r#"(function(){
        var b=document.querySelector("button[type=submit],button");
        if(!b)return "no_button";
        b.click();
        return "clicked";
    })()"#;
    let _ = cdp_eval(ws_url, 7, click_script).await?;

    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    let get_text_script = r#"(function(){
        return JSON.stringify({
            title: document.title,
            url: location.href,
            text: document.body ? document.body.innerText.slice(0,3000) : ""
        });
    })()"#;
    let val = cdp_eval(ws_url, 8, get_text_script).await?;
    let text = val.get("value").and_then(|v| v.as_str()).unwrap_or("");

    Ok(serde_json::json!({"action": "login", "url": url, "result": text, "diag": diag}))
}
