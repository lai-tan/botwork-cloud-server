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

mod scheduler;

#[derive(Debug, Parser)]
#[command(version, name = "botwork-cloud-server")]
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

/// 解析 BOTWORK_PROVIDERS env 并为每条上游做 API 类型检测，
/// 构造多上游路由表。空/解析失败返回空 Vec（调用方回退单上游现状逻辑）。
async fn build_multi_upstreams(default_api_key: &str) -> Vec<adapter_shared::adapter::UpstreamProvider> {
    let raw = match std::env::var("BOTWORK_PROVIDERS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Vec::new(),
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
            strict_config,
            remote_control,
            managed_daemon,
        } = CloudServerArgs::parse();

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
            let multi = build_multi_upstreams(api_key).await;
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
            std::env::set_var("OPENAI_API_KEY", api_key);
            std::env::set_var("AGNES_API_KEY", api_key);
        }

        // --claude-code 模式（§16）：启动适配器 + claude 子进程。
        // 适配器做 Anthropic Messages → Chat Completions 协议转换 + 多上游路由。
        // - 有 trailing args（`-- -p "msg" ...`）：one-shot 模式
        // - 无 trailing args + `--listen stdio://`：MCP 长连接模式
        if claude_code {

            // 确保适配器已启动（上方 is_chat/multi 分支可能已启动）
            if adapter.is_none() {
                if let (Some(api_key), Some(base_url)) = (&api_key, &base_url) {
                    std::env::set_var("OPENAI_API_KEY", api_key);
                    std::env::set_var("AGNES_API_KEY", api_key);

                    let multi = build_multi_upstreams(api_key).await;
                    if multi.len() >= 2 {
                        adapter = Some(
                            ApiAdapter::start_multi(multi, adapter_port)
                                .await
                                .map_err(std::io::Error::other)?,
                        );
                    } else {
                        let model = claude_model.as_deref().unwrap_or("claude-sonnet-4-20250514");
                        let is_chat = detect_api_type(base_url, api_key, model).await;
                        if is_chat {
                            adapter = Some(
                                ApiAdapter::start(
                                    base_url.clone(),
                                    api_key.clone(),
                                    true,
                                    adapter_port,
                                )
                                .await
                                .map_err(std::io::Error::other)?,
                            );
                        }
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
            if let Some(ref api_key) = api_key {
                claude_cmd.env("ANTHROPIC_API_KEY", api_key);
            } else if std::env::var("ANTHROPIC_API_KEY").is_err() {
                claude_cmd.env("ANTHROPIC_API_KEY", "botwork-adapter");
            }

            // stdio 透传
            claude_cmd
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());

            tracing::info!("Spawning Claude Code: {:?}", claude_cmd);

            let mut child = claude_cmd
                .spawn()
                .map_err(|e| anyhow::anyhow!("Failed to spawn claude: {e}"))?;

            let status = child.wait().await.map_err(|e| {
                anyhow::anyhow!("Claude Code process wait error: {e}")
            })?;

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