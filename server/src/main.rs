// Copyright (c) 2026 Jehu Shaw. All rights reserved.

#![recursion_limit = "256"]


use clap::Parser;
use codex_app_server::AppServerCodeModeHostArgs;
use codex_app_server::AppServerRuntimeOptions;
use codex_app_server::AppServerTransport;

use codex_app_server::run_main_with_transport_options;
use codex_arg0::Arg0DispatchPaths;
use codex_arg0::arg0_dispatch_or_else;
use codex_config::LoaderOverrides;
use codex_protocol::protocol::SessionSource;
use codex_utils_cli::CliConfigOverrides;
use codex_websocket_auth::WebsocketAuthArgs;


use adapter_shared::adapter::ApiAdapter;
use adapter_shared::api_detect::detect_api_type;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

mod credentials;
mod scheduler;

/// --version 输出（AGENT_GATEWAY_ARCHITECTURE_FIX_DESIGN §4.5-3）：
/// `botwork-cloud-server 0.2.0 (codex 0.161.0)`。安装脚本 parse_version_output
/// 取第二字段（split_whitespace().nth(1)），第三字段为超集，现有检测逻辑兼容。
/// vendored codex 上游版本（0.161.0）人工维护：vendored 目录无 .git tag /
/// npm 版本号（workspace version 均为 0.0.0），无法自动提取——vendored patch
/// 更新时同步更新此字面量（协议重大变更时两侧同步升级为人工评审点，§4.5-2）。
/// （concat! 仅接受字面量，故 codex 版本号内嵌于本字符串）
const VERSION_OUTPUT: &str = concat!(env!("CARGO_PKG_VERSION"), " (codex 0.161.0)");

#[derive(Debug, Parser)]
#[command(version = VERSION_OUTPUT, name = "botwork-cloud-server")]
struct CloudServerArgs {
    #[command(flatten)]
    config_overrides: CliConfigOverrides,

    #[command(flatten)]
    code_mode_host: AppServerCodeModeHostArgs,

    /// Transport endpoint URL. Supported values: `stdio://` (default),
    /// `unix://`, `unix://PATH`, `ws://IP:PORT`, `off`.
    #[arg(
        long = "listen",
        value_name = "URL",
        default_value = AppServerTransport::DEFAULT_LISTEN_URL
    )]
    listen: AppServerTransport,

    /// Session source used to derive product restrictions and metadata.
    #[arg(
        long = "session-source",
        value_name = "SOURCE",
        default_value = "vscode",
        value_parser = SessionSource::from_startup_arg
    )]
    session_source: SessionSource,

    #[command(flatten)]
    auth: WebsocketAuthArgs,

    /// Schedule daemon mode (`--schedule`, mutually exclusive with a non-default
    /// `--listen`): runs the automation scheduler in-process. See
    /// docs/AUTOMATION-DESIGN.md §5.2.
    #[command(flatten)]
    schedule: scheduler::ScheduleArgs,

    /// API key for the model provider. Passed to the built-in adapter for
    /// API type detection and proxy forwarding. Internally set as the
    /// `OPENAI_API_KEY` environment variable so codex app-server's `env_key`
    /// can read it.
    #[arg(long = "api-key", value_name = "KEY", env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    /// Upstream base URL for API type detection. When provided, botwork-cloud-server
    /// checks whether the upstream is Chat Completions or Responses API. If Chat
    /// Completions, a built-in adapter is started to convert between the two.
    #[arg(long = "base-url", value_name = "URL")]
    base_url: Option<String>,

    /// Fixed port for the built-in adapter. When omitted, a random port is
    /// used. Set this to a fixed value (e.g. 8082) so that claude code CLI
    /// can use a predictable ANTHROPIC_BASE_URL.
    #[arg(long = "adapter-port", value_name = "PORT", env = "ADAPTER_PORT")]
    adapter_port: Option<u16>,

    /// Claude Code 模式（§16）：启动内置适配器 + claude 子进程。
    /// 与 codex app-server 模式互斥。适配器做 Anthropic Messages → Chat
    /// Completions 协议转换 + 多上游路由（BOTWORK_PROVIDERS env）。
    /// - 有 trailing args（`-- -p "msg" ...`）：one-shot 模式
    /// - 无 trailing args + `--listen stdio://`：MCP 长连接模式
    #[arg(long = "claude-code", default_value_t = false)]
    claude_code: bool,

    /// Claude Code 子进程的启动命令（默认 "claude"）。
    #[arg(long = "claude-bin", value_name = "PATH", default_value = "claude")]
    claude_bin: String,

    /// Claude Code 的模型参数（传给 `claude mcp serve --model` 或
    /// `claude -p ... --model`）。为空时用 claude 默认模型。
    #[arg(long = "claude-model", value_name = "MODEL")]
    claude_model: Option<String>,

    /// Trailing arguments passed to claude (after `--`). Used with
    /// `--claude-code` for one-shot mode.
    #[arg(last = true, allow_hyphen_values = true)]
    claude_args: Vec<String>,

    /// OpenClaw 网关包装模式（AGENT_GATEWAY_ARCHITECTURE_FIX_DESIGN §4.1）：
    /// `sh -c --exec <cmd>` 执行 + 审计 + 超时保护。cloud-server 不理解
    /// openclaw 参数语义，仅作执行器；stdio 全透传。当前仅支持 "openclaw"。
    #[arg(long = "mode", value_name = "MODE")]
    mode: Option<String>,

    /// 包装模式待执行命令（--mode openclaw 时必填），整条 openclaw 命令
    /// （含 config patch 管道）由 bot-work-windows shell_escape 后传入。
    #[arg(long = "exec", value_name = "CMD")]
    exec: Option<String>,

    /// 审计元数据 JSON（{model, cwd}），原样写入审计日志（不解析语义）。
    #[arg(long = "audit-meta", value_name = "JSON")]
    audit_meta: Option<String>,

    /// 包装模式超时秒数（默认 600，openclaw 长任务保护）。
    #[arg(long = "exec-timeout", value_name = "SECS", default_value_t = 600)]
    exec_timeout: u64,

    /// Fail if config.toml contains unknown configuration fields.
    #[arg(long = "strict-config", default_value_t = false)]
    strict_config: bool,

    /// Enable remote control for this app-server process.
    #[arg(long = "remote-control", hide = true)]
    remote_control: bool,

    /// Save loaded threads during managed daemon shutdown.
    #[arg(long, hide = true)]
    managed_daemon: bool,
}

/// 多上游 env 表条目（DESIGN-ChatModelSelector §9.3 层 2「远端 adapter 动态多上游路由」）：
/// 本地把 agent.available_models 的全部 provider 序列化传入 env（SSH 加密通道，
/// 与现有 api_key env 同等暴露面）；远端解析后按请求模型名路由到对应上游。
/// models 兼容 string（旧形态）与 {name,...}（对象形态，§11.3）两种条目。
#[derive(serde::Deserialize)]
struct EnvProvider {
    #[serde(rename = "baseUrl")]
    base_url: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    #[serde(default)]
    models: Vec<serde_json::Value>,
}

impl EnvProvider {
    fn model_names(&self) -> Vec<String> {
        self.models
            .iter()
            .filter_map(|m| match m {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(o) => o
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                _ => None,
            })
            .collect()
    }
}

/// 解析 BOTWORK_PROVIDERS 并为每条上游做 API 类型检测，构造多上游路由表。
/// `providers_override`：§4.3-5c 握手通道下由 stdin credentials 传入（不读 env，
/// cloud-server 自身 environ 不含凭据）；`None` 时回退读 env（Env 回退路径）。
/// 空/解析失败返回空 Vec（调用方回退单上游现状逻辑）。
async fn build_multi_upstreams(
    default_api_key: &str,
    providers_override: Option<&str>,
) -> Vec<adapter_shared::adapter::UpstreamProvider> {
    let raw = if let Some(p) = providers_override {
        if p.trim().is_empty() {
            return Vec::new();
        }
        p.to_string()
    } else {
        match std::env::var("BOTWORK_PROVIDERS") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => return Vec::new(),
        }
    };
    let providers: Vec<EnvProvider> = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("BOTWORK_PROVIDERS 解析失败（忽略，回退单上游）: {e}");
            return Vec::new();
        }
    };
    let mut upstreams = Vec::new();
    for p in providers {
        if p.base_url.trim().is_empty() {
            continue;
        }
        let models = p.model_names();
        if models.is_empty() {
            continue;
        }
        let api_key = if p.api_key.is_empty() {
            default_api_key.to_string()
        } else {
            p.api_key
        };
        let probe_model = models[0].clone();
        let is_chat = detect_api_type(&p.base_url, &api_key, &probe_model).await;
        tracing::info!(
            "multi-upstream provider {} ({} models, is_chat={})",
            p.base_url,
            models.len(),
            is_chat
        );
        upstreams.push(adapter_shared::adapter::UpstreamProvider {
            base_url: p.base_url,
            api_key,
            is_chat,
            models,
        });
    }
    upstreams
}

/// §4.3-5c claude 模式 stdin 握手消费：
/// - one-shot（`is_one_shot = true`）：读 stdin 首行，`parse_credentials_first_line`
///   消费 magic 行；非 magic 行缓冲返回（env 回退下 stdin 可能有内容）
/// - 长连接（`is_one_shot = false`）：读 stdin 行级直到 `initialize` 消费 credentials，
///   缓冲所有读到的行（含剥离后的 initialize），返回给调用方转发给 claude 子进程
///
/// 返回 `(credentials, buffered_lines)`。`credentials = None` 表示未消费到
/// （env 回退或超时），调用方按 args/env 回退处理。30s 超时防客户端不发 initialize。
async fn consume_claude_credentials(
    is_one_shot: bool,
) -> std::io::Result<(Option<HashMap<String, String>>, Vec<String>)> {
    let mut stdin = tokio::io::stdin();
    let mut reader = BufReader::new(&mut stdin);
    let mut buffered: Vec<String> = Vec::new();

    if is_one_shot {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok((None, buffered));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        match credentials::parse_credentials_first_line(trimmed) {
            Ok(Some(creds)) => return Ok((Some(creds), buffered)),
            Ok(None) => {
                buffered.push(trimmed.to_string());
                return Ok((None, buffered));
            }
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "credentials first line: {e:?}"
                )));
            }
        }
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok((None, buffered));
        }
        let remaining = deadline - now;
        let mut line = String::new();
        let n = match tokio::time::timeout(remaining, reader.read_line(&mut line)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Ok((None, buffered)),
        };
        if n == 0 {
            return Ok((None, buffered));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        match credentials::consume_initialize_credentials(trimmed) {
            Ok(credentials::ConsumeOutcome::Consumed {
                credentials,
                forwarded_line,
            }) => {
                buffered.push(forwarded_line);
                return Ok((Some(credentials), buffered));
            }
            Ok(credentials::ConsumeOutcome::Passthrough) => {
                buffered.push(trimmed.to_string());
            }
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "credentials initialize: {e:?}"
                )));
            }
        }
    }
}

fn read_active_provider() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let config_path = format!("{home}/.codex/config.toml");
    let content = std::fs::read_to_string(&config_path).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("model_provider") {
            let val = val.trim();
            if let Some(val) = val.strip_prefix('=') {
                let val = val.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

fn main() -> anyhow::Result<()> {
    let remote_control_disabled = codex_app_server::take_remote_control_disabled_env();
    arg0_dispatch_or_else(move |arg0_paths: Arg0DispatchPaths| async move {
        let CloudServerArgs {
            config_overrides,
            code_mode_host,
            listen,
            session_source,
            auth,
            schedule,
            api_key,
            base_url,
            adapter_port,
            claude_code,
            claude_bin,
            claude_model,
            claude_args,
            mode,
            exec,
            audit_meta,
            exec_timeout,
            strict_config,
            remote_control,
            managed_daemon,
        } = CloudServerArgs::parse();

        // §4.1 OpenClaw 网关包装模式：与 codex/claude 模式互斥，early return。
        if mode.as_deref() == Some("openclaw") {
            let exec = exec
                .ok_or_else(|| anyhow::anyhow!("--mode openclaw requires --exec <CMD>"))?;
            return run_openclaw_exec(exec, audit_meta, exec_timeout).await;
        }
        if mode.is_some() {
            anyhow::bail!("--mode 仅支持 openclaw");
        }
        if exec.is_some() || audit_meta.is_some() {
            anyhow::bail!("--exec/--audit-meta 须与 --mode openclaw 搭配使用");
        }

        let loader_overrides = LoaderOverrides::default();
        let transport = listen;
        let auth = auth.try_into_settings()?;

        let mut runtime_options = AppServerRuntimeOptions {
            code_mode_host_transport: code_mode_host.into(),
            managed_daemon,
            ..Default::default()
        };
        runtime_options.remote_control_startup_mode =
            match (remote_control, remote_control_disabled) {
                (true, _) => codex_app_server::RemoteControlStartupMode::EnabledEphemeral,
                (false, true) => codex_app_server::RemoteControlStartupMode::DisabledEphemeral,
                (false, false) => codex_app_server::RemoteControlStartupMode::ResolvePersisted,
            };

        let mut effective_config_overrides = config_overrides;
        let mut adapter: Option<ApiAdapter> = None;

        if let (Some(api_key), Some(base_url)) = (&api_key, &base_url) {
            // §4.3-5d env 回退路径（--api-key 有值）：codex app-server 为进程内
            // 代码读 cloud-server env，env_key 机制要求 OPENAI_API_KEY 存在。
            // handshake 路径（api_key 无值）不进入此分支，codex 经 vendored
            // InitializeParams.credentials 获取 key（§4.3-5e）。adapter 启动后
            // codex base_url 指向 adapter，env key 仅满足 codex 启动前置。
            std::env::set_var("OPENAI_API_KEY", api_key);
            std::env::set_var("AGNES_API_KEY", api_key);

            let overrides = effective_config_overrides.parse_overrides().unwrap_or_default();

            let model = overrides
                .iter()
                .find(|(k, _)| k == "model")
                .and_then(|(_, v)| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| "gpt-4o".to_string());

            // 批次 4 多上游路由（BOTWORK_PROVIDERS env，≥2 条生效）：
            // 无论上游是 Responses 还是 Chat 一律经 adapter 分发——codex 的
            // base_url 只有一个，跨 provider 路由只能靠 adapter 按请求模型名完成。
            // 单条/未配置时维持下方单上游现状逻辑（Responses 直连优化保留）。
            let multi = build_multi_upstreams(api_key, None).await;
            if multi.len() >= 2 {
                tracing::info!(
                    "Starting multi-upstream adapter with {} providers",
                    multi.len()
                );
                adapter = Some(
                    ApiAdapter::start_multi(multi, adapter_port)
                        .await
                        .map_err(std::io::Error::other)?,
                );

                let adapter_url = adapter.as_ref().unwrap().local_base_url();
                tracing::info!("Multi-upstream adapter running at {}", adapter_url);

                let provider = read_active_provider().unwrap_or_else(|| "openai".to_string());
                tracing::info!("Active provider: {}", provider);

                effective_config_overrides
                    .raw_overrides
                    .push(format!("model_providers.{provider}.base_url={adapter_url}"));

                effective_config_overrides
                    .raw_overrides
                    .push(format!("model_providers.{provider}.supports_websockets=false"));
            } else {
                let is_chat = detect_api_type(base_url, api_key, &model).await;

                if is_chat {
                    tracing::info!(
                        "Detected Chat Completions API, starting built-in adapter for {}",
                        base_url
                    );

                    adapter = Some(
                        ApiAdapter::start(base_url.clone(), api_key.clone(), true, adapter_port)
                            .await
                            .map_err(std::io::Error::other)?,
                    );

                    let adapter_url = adapter.as_ref().unwrap().local_base_url();
                    tracing::info!("Adapter running at {}", adapter_url);

                    let provider = read_active_provider().unwrap_or_else(|| "openai".to_string());
                    tracing::info!("Active provider: {}", provider);

                    effective_config_overrides
                        .raw_overrides
                        .push(format!("model_providers.{provider}.base_url={adapter_url}"));

                    effective_config_overrides
                        .raw_overrides
                        .push(format!("model_providers.{provider}.supports_websockets=false"));
                }
            }
        } else if let Some(api_key) = &api_key {
            // §4.3-5d env 回退路径（无 base_url 但有 api_key）：同上 set_var
            // 保留，handshake 路径不进入此分支。
            std::env::set_var("OPENAI_API_KEY", api_key);
            std::env::set_var("AGNES_API_KEY", api_key);
        } else {
            // §4.3-5e handshake 路径（无 --api-key 无 --base-url）：
            // credentials 经 vendored InitializeParams.credentials 从 initialize
            // 握手到达。预绑定端口 → 设 config override 指向 adapter → spawn
            // 任务待 credentials 到达后启动 adapter。
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| anyhow::anyhow!("failed to pre-bind adapter port: {e}"))?;
            let handshake_port = listener
                .local_addr()
                .map_err(|e| anyhow::anyhow!("failed to get pre-bound port: {e}"))?
                .port();
            drop(listener);

            let adapter_url = format!("http://127.0.0.1:{handshake_port}/v1");
            let provider = read_active_provider().unwrap_or_else(|| "openai".to_string());
            effective_config_overrides
                .raw_overrides
                .push(format!("model_providers.{provider}.base_url={adapter_url}"));
            effective_config_overrides
                .raw_overrides
                .push(format!("model_providers.{provider}.supports_websockets=false"));
            tracing::info!(
                "Handshake mode: adapter will start at {} when credentials arrive",
                adapter_url
            );

            let (cred_tx, cred_rx) = tokio::sync::oneshot::channel();
            codex_app_server::set_credential_bridge(cred_tx);

            let overrides = effective_config_overrides.parse_overrides().unwrap_or_default();
            let model = overrides
                .iter()
                .find(|(k, _)| k == "model")
                .and_then(|(_, v)| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| "gpt-4o".to_string());

            tokio::spawn(async move {
                let creds = match cred_rx.await {
                    Ok(c) => c,
                    Err(_) => {
                        tracing::warn!(
                            "Credential bridge closed without receiving credentials"
                        );
                        return;
                    }
                };
                let api_key = creds
                    .get("OPENAI_API_KEY")
                    .cloned()
                    .unwrap_or_default();
                let providers_override = creds.get("BOTWORK_PROVIDERS").cloned();

                let multi =
                    build_multi_upstreams(&api_key, providers_override.as_deref()).await;
                let multi_count = multi.len();
                if multi_count >= 2 {
                    match ApiAdapter::start_multi(multi, Some(handshake_port)).await {
                        Ok(a) => {
                            tracing::info!(
                                "Handshake multi-upstream adapter at {} ({} providers)",
                                a.local_base_url(),
                                multi_count
                            );
                            let _adapter = a;
                            std::future::pending::<()>().await;
                        }
                        Err(e) => {
                            tracing::error!("Failed to start multi-upstream adapter: {e}")
                        }
                    }
                } else {
                    let base_url = std::env::var("OPENAI_BASE_URL")
                        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
                    let is_chat = detect_api_type(&base_url, &api_key, &model).await;
                    match ApiAdapter::start(
                        base_url,
                        api_key,
                        is_chat,
                        Some(handshake_port),
                    )
                    .await
                    {
                        Ok(a) => {
                            tracing::info!(
                                "Handshake single-upstream adapter at {} (chat={})",
                                a.local_base_url(),
                                is_chat
                            );
                            let _adapter = a;
                            std::future::pending::<()>().await;
                        }
                        Err(e) => tracing::error!("Failed to start adapter: {e}"),
                    }
                }
            });
        }

        // --claude-code 模式（§16）：启动适配器 + claude 子进程。
        // 适配器做 Anthropic Messages → Chat Completions 协议转换 + 多上游路由。
        // - 有 trailing args（`-- -p "msg" ...`）：one-shot 模式
        // - 无 trailing args + `--listen stdio://`：MCP 长连接模式
        if claude_code {
            // §4.3-5c 凭据来源决策：
            // - api_key 有值（--api-key 传入或 OPENAI_API_KEY env）：Env 回退路径，
            //   adapter 可能已由上方 codex 段启动，stdio inherit 透传
            // - api_key 无值：stdin 握手通道，消费 credentials 后启动 adapter +
            //   延迟 spawn claude（env 注入，不 set_var），piped stdio 转发
            let is_handshake = api_key.is_none();
            let mut handshake_creds: Option<HashMap<String, String>> = None;
            let mut buffered_lines: Vec<String> = Vec::new();

            if is_handshake {
                let is_one_shot = !claude_args.is_empty();
                let (creds, buf) = consume_claude_credentials(is_one_shot)
                    .await
                    .map_err(|e| anyhow::anyhow!("credentials handshake: {e}"))?;
                handshake_creds = creds;
                buffered_lines = buf;
            }

            // 提取有效凭据（握手优先，回退 args）
            let effective_api_key = handshake_creds
                .as_ref()
                .and_then(|c| c.get("OPENAI_API_KEY"))
                .cloned()
                .or(api_key.clone());
            let providers_override = handshake_creds
                .as_ref()
                .and_then(|c| c.get("BOTWORK_PROVIDERS"))
                .map(|s| s.as_str());

            // 确保适配器已启动（上方 codex 段可能已启动）
            if adapter.is_none() {
                let key_ref = effective_api_key.as_deref().unwrap_or("botwork-adapter");
                let multi = build_multi_upstreams(key_ref, providers_override).await;
                if multi.len() >= 2 {
                    tracing::info!(
                        "Starting multi-upstream adapter with {} providers",
                        multi.len()
                    );
                    adapter = Some(
                        ApiAdapter::start_multi(multi, adapter_port)
                            .await
                            .map_err(std::io::Error::other)?,
                    );
                } else if let Some(ref base_url) = base_url {
                    let model = claude_model.as_deref().unwrap_or("claude-sonnet-4-20250514");
                    let is_chat = detect_api_type(base_url, key_ref, model).await;
                    if is_chat {
                        tracing::info!(
                            "Detected Chat Completions API, starting built-in adapter for {}",
                            base_url
                        );
                        adapter = Some(
                            ApiAdapter::start(
                                base_url.clone(),
                                key_ref.to_string(),
                                true,
                                adapter_port,
                            )
                            .await
                            .map_err(std::io::Error::other)?,
                        );
                    }
                }
            }

            // 设置 ANTHROPIC_BASE_URL 指向适配器（或直连上游）
            // Claude Code SDK 追加 `/v1/messages`，所以 base URL 不能含 `/v1`
            let anthropic_base_url = if let Some(ref adapter) = adapter {
                adapter.local_base_url_no_v1()
            } else if let Some(ref base_url) = base_url {
                base_url.clone()
            } else {
                "https://api.anthropic.com".to_string()
            };
            std::env::set_var("ANTHROPIC_BASE_URL", &anthropic_base_url);
            tracing::info!("Claude Code mode: ANTHROPIC_BASE_URL = {}", anthropic_base_url);

            // 构造 claude 子进程命令
            let mut claude_cmd = tokio::process::Command::new(&claude_bin);

            if !claude_args.is_empty() {
                // one-shot 模式：trailing args 直接传给 claude
                claude_cmd.args(&claude_args);
            } else {
                // MCP 长连接模式：claude mcp serve [--model X]
                claude_cmd.arg("mcp").arg("serve");
                if let Some(ref model) = claude_model {
                    claude_cmd.arg("--model").arg(model);
                }
            }

            claude_cmd.env("ANTHROPIC_BASE_URL", &anthropic_base_url);

            // Claude Code 需要 ANTHROPIC_API_KEY 才能跳过 /login 检查。
            // 适配器不校验 key，用真实 key 或占位符均可。
            if let Some(ref key) = effective_api_key {
                claude_cmd.env("ANTHROPIC_API_KEY", key);
            } else if std::env::var("ANTHROPIC_API_KEY").is_err() {
                claude_cmd.env("ANTHROPIC_API_KEY", "botwork-adapter");
            }

            // §4.3-5c stdio 策略：
            // - Env 回退：inherit（现有行为，stdin 全是业务内容直接透传）
            // - 握手通道：piped（消费后转发缓冲行 + 后续透传 + stdout 转发）
            let status = if is_handshake {
                claude_cmd
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::inherit());

                tracing::info!("Spawning Claude Code (handshake): {:?}", claude_cmd);
                let mut child = claude_cmd
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("Failed to spawn claude: {e}"))?;

                let mut child_stdin = child.stdin.take().unwrap();
                let mut child_stdout = child.stdout.take().unwrap();

                // 写缓冲行（消费 credentials 后剩余的行，含剥离后的 initialize）
                for line in &buffered_lines {
                    child_stdin
                        .write_all(format!("{line}\n").as_bytes())
                        .await
                        .map_err(|e| anyhow::anyhow!("write buffered line: {e}"))?;
                }

                // stdin 透传 + stdout 透传
                let stdin_handle = tokio::spawn(async move {
                    let mut stdin = tokio::io::stdin();
                    let _ = tokio::io::copy(&mut stdin, &mut child_stdin).await;
                });
                let stdout_handle = tokio::spawn(async move {
                    let mut stdout = tokio::io::stdout();
                    let _ = tokio::io::copy(&mut child_stdout, &mut stdout).await;
                });

                let status = child
                    .wait()
                    .await
                    .map_err(|e| anyhow::anyhow!("Claude Code process wait error: {e}"))?;

                let _ = stdin_handle.await;
                let _ = stdout_handle.await;
                status
            } else {
                claude_cmd
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit());

                tracing::info!("Spawning Claude Code: {:?}", claude_cmd);
                let mut child = claude_cmd
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("Failed to spawn claude: {e}"))?;

                child
                    .wait()
                    .await
                    .map_err(|e| anyhow::anyhow!("Claude Code process wait error: {e}"))?
            };

            if let Some(adapter) = adapter {
                adapter.stop().await;
            }

            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
            return Ok(());
        }

        // --schedule 调度守护模式（与 --listen 非默认值互斥）：
        // 复用上方适配器/环境变量前置，但不进入 transport 循环，
        // 而是运行调度器 + 管理 API + 优雅停机（AUTOMATION-DESIGN.md §5.2）
        if schedule.is_enabled() {
            if transport != AppServerTransport::Stdio {
                anyhow::bail!("--schedule 与非默认 --listen 互斥（调度守护进程不监听 transport）");
            }
            return scheduler::run_schedule_daemon(scheduler::ScheduleDaemonOptions {
                arg0_paths,
                cli_config_overrides: effective_config_overrides,
                loader_overrides,
                strict_config,
                session_source,
                schedule_args: schedule,
                adapter,
            })
            .await;
        }

        let exit = run_main_with_transport_options(
            arg0_paths,
            effective_config_overrides,
            loader_overrides,
            strict_config,
            false,
            transport,
            session_source,
            auth,
            runtime_options,
        )
        .await?;

        if let Some(adapter) = adapter {
            adapter.stop().await;
        }

        if exit == codex_app_server::AppServerExit::Forced {
            std::process::exit(0);
        }
        Ok(())
    })
}
// ── §4.1 OpenClaw 网关包装模式（AGENT_GATEWAY_ARCHITECTURE_FIX_DESIGN §4.1）──

/// FNV-1a 64 位哈希（诊断对照用，非密码学）：两侧（bot-work-windows /
/// cloud-server）同算法保证跨版本一致，仅用于审计行与客户端 debug 日志的
/// 命令哈希对照。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// bot-work 数据目录（审计日志落点）：BOT_WORK_DIR 优先，否则 $HOME/bot-work
/// （与 AUTO_INSTALL 部署目录同域，默认 /root/bot-work）。
fn bot_work_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("BOT_WORK_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    std::path::PathBuf::from(home).join("bot-work")
}

/// 审计日志：追加写 <bot_work_dir>/logs/openclaw-audit.log，>10MB 截断
/// （沿用 AUTO_INSTALL watchdog 截断先例）。写失败不阻塞执行（审计尽力而为，
/// 不记录明文命令，防聊天内容入日志）。
fn write_openclaw_audit(line: &serde_json::Value) {
    let log_dir = bot_work_dir().join("logs");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let path = log_dir.join("openclaw-audit.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 10 * 1024 * 1024 {
            let _ = std::fs::write(&path, b"");
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

/// §4.1 包装模式执行体：`sh -c <cmd>` + 超时保护 + 审计行。stdio 全透传
/// （cloud-server 不解析 openclaw JSON，stdout 原样回 SSH channel）。
async fn run_openclaw_exec(
    exec: String,
    audit_meta: Option<String>,
    exec_timeout: u64,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let cmd_hash = format!("{:016x}", fnv1a64(exec.as_bytes()));
    let meta: serde_json::Value = audit_meta
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    let cwd = meta
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let model = meta
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // §4.3-5d stdin 首行消费：openclaw one-shot 模式，stdin 首行
    // BOTWORK_CREDENTIALS:{json}（bot-work-windows 发送）。消费后三个 key
    // 注入 sh 子进程 env（不 set_var，仅子进程启动时刻存在）。
    // openclaw 无后续 stdin 载荷（ssh/agent.rs writer 立即 eof），stdin
    // piped 立即关闭。
    let mut stdin_reader = BufReader::new(tokio::io::stdin());
    let mut first_line = String::new();
    stdin_reader
        .read_line(&mut first_line)
        .await
        .map_err(|e| anyhow::anyhow!("read stdin first line: {e}"))?;

    let mut child_cmd = tokio::process::Command::new("sh");
    child_cmd
        .arg("-c")
        .arg(&exec)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let trimmed = first_line.trim_end_matches(['\r', '\n']);
    match credentials::parse_credentials_first_line(trimmed) {
        Ok(Some(creds)) => {
            for key in credentials::CREDENTIAL_KEYS.iter() {
                if let Some(val) = creds.get(*key) {
                    child_cmd.env(key, val);
                }
            }
            tracing::info!("openclaw exec: consumed credentials from stdin first line");
        }
        Ok(None) => {
            tracing::warn!(
                "openclaw exec: stdin first line is not credentials magic, env fallback"
            );
        }
        Err(e) => {
            return Err(anyhow::anyhow!("openclaw credentials parse: {e:?}"));
        }
    }

    let mut child = child_cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn sh -c: {e}"))?;
    // openclaw 无后续 stdin 载荷，立即关闭子进程 stdin
    drop(child.stdin.take());

    let exit_code = match tokio::time::timeout(
        std::time::Duration::from_secs(exec_timeout),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) => status.code().unwrap_or(-1),
        Ok(Err(e)) => anyhow::bail!("wait child: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            tracing::warn!("openclaw exec timeout after {exec_timeout}s, cmd_hash={cmd_hash}");
            -1
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    write_openclaw_audit(&serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "model": model,
        "cwd": cwd,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
        "cmd_hash": cmd_hash,
    }));
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

#[cfg(test)]
mod openclaw_exec_tests {
    use super::*;

    #[test]
    fn test_fnv1a64_stable() {
        // 与 bot-work-windows 侧 openclaw_adapter::fnv1a64 同算法同期望值
        assert_eq!(format!("{:016x}", fnv1a64(b"")), "cbf29ce484222325");
        assert_eq!(format!("{:016x}", fnv1a64(b"echo hi")), "b0f350cd92f2777f");
    }

    #[test]
    fn test_bot_work_dir_env_override() {
        std::env::set_var("BOT_WORK_DIR", "/tmp/bw-test");
        assert_eq!(bot_work_dir(), std::path::PathBuf::from("/tmp/bw-test"));
        std::env::remove_var("BOT_WORK_DIR");
    }
}