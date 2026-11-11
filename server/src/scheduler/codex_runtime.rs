// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! codex app-server 进程内运行时封装（AUTOMATION-DESIGN.md §5.2）：
//! - 组装 `InProcessClientStartArgs`（ConfigBuilder 加载 ~/.codex/config.toml
//!   + CLI overrides，其中 base_url 已由 main 分支指回内置适配器）；
//! - 事件路由循环：按 thread_id 将 turn/item 通知分发到对应任务的 channel；
//! - 无人值守语义：所有 ServerRequest（审批/表单等）统一拒绝，防止任务挂起。

use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_client::InProcessClientStartArgs;
use codex_app_server_client::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_protocol::openai_models::ReasoningEffort;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_core::config::find_codex_home;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionSource;
use codex_utils_cli::CliConfigOverrides;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::config::Schedule;

/// 分发到任务的事件（仅该 thread 关心的通知）。
#[derive(Debug)]
pub enum RoutedEvent {
    TurnStarted {
        thread_id: String,
        turn_id: String,
    },
    TurnCompleted {
        thread_id: String,
        turn_id: String,
        /// Completed | Interrupted | Failed | InProgress
        status: String,
        turn_json: serde_json::Value,
    },
    AgentMessage {
        thread_id: String,
        text: String,
    },
    /// §4.9 证据源 item（v2.52 F2）：CommandExecution/FileChange/
    /// McpToolCall/WebSearch 四类实质动作项整件透传，由 executor
    /// 映射为 evidence/artifacts（对齐本地调度 3 变体口径）；
    /// AgentMessage 走上方专用变体（output 聚积 + 标记检测）。
    ItemCompleted {
        thread_id: String,
        item: ThreadItem,
    },
}

#[derive(Clone)]
pub struct CodexRuntime {
    handle: codex_app_server_client::InProcessAppServerRequestHandle,
    routes: Arc<DashMap<String, mpsc::Sender<RoutedEvent>>>,
    request_seq: Arc<AtomicI64>,
    shutdown_token: CancellationToken,
    /// 本机活跃 provider（config.model_provider_id）：交接 resume 的
    /// rollout meta 携带导出方 provider（如本地 openai），远程无该
    /// provider 定义时会落到内置 openai 后端（chatgpt.com，境内被
    /// DNS 污染不可达）——resume 显式覆盖为本机活跃 provider。
    active_provider: String,
}

impl CodexRuntime {
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        arg0_paths: codex_arg0::Arg0DispatchPaths,
        cli_overrides: CliConfigOverrides,
        loader_overrides: LoaderOverrides,
        strict_config: bool,
        session_source: SessionSource,
        shutdown_token: CancellationToken,
    ) -> Result<Self> {
        let cli_kv = cli_overrides
            .parse_overrides()
            .map_err(|e| anyhow::anyhow!("解析 -c 覆盖项失败: {e}"))?;
        let codex_home = find_codex_home().context("定位 CODEX_HOME 失败")?;

        let config = match ConfigBuilder::default()
            .codex_home(codex_home.to_path_buf())
            .cli_overrides(cli_kv.clone())
            .loader_overrides(loader_overrides)
            .strict_config(strict_config)
            .cloud_config_bundle(CloudConfigBundleLoader::default())
            .build()
            .await
        {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(error = ?err, "加载用户配置失败，回退默认配置");
                codex_core::config::Config::load_default_with_cli_overrides_for_codex_home(
                    codex_home.to_path_buf(),
                    cli_kv.clone(),
                )
                .await
                .context("回退默认配置失败")?
            }
        };
        let config = Arc::new(config);
        let active_provider = config.model_provider_id.clone();

        let embedded_network_policy =
            codex_app_server_client::EmbeddedNetworkPolicy::default();
        let local_runtime_paths = ExecServerRuntimePaths::from_optional_paths(
            arg0_paths.codex_self_exe.clone(),
            arg0_paths.codex_linux_sandbox_exe.clone(),
        )
        .map_err(std::io::Error::other)
        .context("解析 exec-server 运行时路径失败")?;
        let environment_manager = EnvironmentManager::from_codex_home(
            config.codex_home.clone(),
            Some(local_runtime_paths),
            embedded_network_policy.bind(config.http_client_factory()),
        )
        .await
        .map_err(std::io::Error::other)
        .context("初始化 EnvironmentManager 失败")?;
        let state_db = codex_core::init_state_db(&config).await;

        let start_args = InProcessClientStartArgs {
            arg0_paths,
            config: config.clone(),
            cli_overrides: cli_kv,
            loader_overrides: LoaderOverrides::default(),
            strict_config,
            cloud_config_bundle: CloudConfigBundleLoader::default(),
            embedded_network_policy,
            feedback: CodexFeedback::new(),
            log_db: None,
            state_db,
            environment_manager: Arc::new(environment_manager),
            config_warnings: Vec::new(),
            session_source,
            enable_codex_api_key_env: true,
            client_name: "botwork-cloud-schedule".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
        };

        let mut client = InProcessAppServerClient::start(start_args)
            .await
            .map_err(std::io::Error::other)
            .context("启动进程内 codex app-server 运行时失败")?;
        let handle = client.request_handle();

        let routes: Arc<DashMap<String, mpsc::Sender<RoutedEvent>>> = Arc::new(DashMap::new());
        let loop_routes = routes.clone();
        let loop_token = shutdown_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = loop_token.cancelled() => {
                        let _ = client.shutdown().await;
                        break;
                    }
                    ev = client.next_event() => {
                        let Some(ev) = ev else { break };
                        match ev {
                            codex_app_server_client::InProcessServerEvent::ServerNotification(n) => {
                                route_notification(&loop_routes, *n);
                            }
                            codex_app_server_client::InProcessServerEvent::ServerRequest(r) => {
                                // 无人值守：审批/表单等服务器请求统一拒绝，防止任务挂起
                                let _ = client
                                    .reject_server_request(
                                        r.id().clone(),
                                        codex_app_server_protocol::JSONRPCErrorError {
                                            code: -32000,
                                            message: "unattended schedule mode: server request rejected"
                                                .to_string(),
                                            data: None,
                                        },
                                    )
                                    .await;
                            }
                            codex_app_server_client::InProcessServerEvent::Lagged { skipped } => {
                                tracing::warn!("事件消费落后，丢弃 {skipped} 条通知");
                            }
                        }
                    }
                }
            }
            tracing::info!("codex 事件路由循环退出");
        });

        Ok(Self {
            handle,
            routes,
            request_seq: Arc::new(AtomicI64::new(1)),
            shutdown_token,
            active_provider,
        })
    }

    fn next_request_id(&self) -> RequestId {
        RequestId::Integer(self.request_seq.fetch_add(1, Ordering::Relaxed))
    }

    pub fn register_thread(&self, thread_id: &str, tx: mpsc::Sender<RoutedEvent>) {
        self.routes.insert(thread_id.to_string(), tx);
    }

    pub fn unregister_thread(&self, thread_id: &str) {
        self.routes.remove(thread_id);
    }

    /// thread/start：无人值守统一 approval=Never；sandbox 按 permission_level。
    pub async fn thread_start(&self, schedule: &Schedule) -> Result<ThreadStartResponse> {
        let sandbox = if schedule.permission_level == "full_access" {
            codex_protocol::config_types::SandboxMode::DangerFullAccess
        } else {
            codex_protocol::config_types::SandboxMode::WorkspaceWrite
        };
        let params = ThreadStartParams {
            cwd: schedule.work_dir.clone(),
            approval_policy: Some(AskForApproval::Never.into()),
            sandbox: Some(sandbox.into()),
            ..Default::default()
        };
        let request = ClientRequest::ThreadStart {
            request_id: self.next_request_id(),
            params,
        };
        self.handle
            .request_typed(request)
            .await
            .map_err(|e| anyhow::anyhow!("thread/start 失败: {e}"))
    }

    /// thread/resume（§5.6.1 步骤 3 交接接管，v2.43）：path 指定时非运行
    /// thread 的 thread_id 被协议忽略（experimental thread/resume.path；
    /// 交接场景 thread 在本地创建、远程无此 id，传空占位）；sandbox/
    /// approval 语义与 thread_start 一致。rollout 文件缺失由服务端报错
    /// → execute_inner 分流处落 run failed（error 注明）。
    ///
    /// v2.46 修复：paginated 模式 rollout（本地新版 codex 默认）的 path
    /// resume 会按文件 meta 里的 thread id 反查本机存储（防 stale path）
    /// ——上传文件不在 sessions 目录时 state_db 无记录且文件扫描不可达
    /// → "no rollout found"。resume 前先把 rollout 导入
    /// CODEX_HOME/sessions/YYYY/MM/DD/ 规范路径（幂等），并以导入路径
    /// 作为 resume path（与反查命中的 current path 一致）。
    pub async fn thread_resume(&self, schedule: &Schedule) -> Result<ThreadResumeResponse> {
        let sandbox = if schedule.permission_level == "full_access" {
            codex_protocol::config_types::SandboxMode::DangerFullAccess
        } else {
            codex_protocol::config_types::SandboxMode::WorkspaceWrite
        };
        let imported_path = match schedule.handoff_rollout_path.as_deref() {
            Some(src) => Some(import_handoff_rollout(src).await?),
            None => None,
        };
        let params = ThreadResumeParams {
            thread_id: String::new(),
            path: imported_path,
            cwd: schedule.work_dir.clone(),
            // 交接 rollout meta 携带本地（Windows）workspace root，在远程
            // 主机非绝对路径会被 resume 校验拒绝——显式传空列表覆盖
            // （以本 params 的 cwd 为准），错误提示允许 empty list
            runtime_workspace_roots: Some(Vec::new()),
            // v2.47 修复：rollout meta 的 model_provider 是导出方（本地
            // app 为 openai），远程无 model_providers.openai 定义 → 落到
            // 内置 openai 后端（chatgpt.com，DNS 污染不可达）→ run 卡
            // 重试。协议语义：params 携带 model_provider 时跳过 persisted
            // metadata 的 provider 回填，以本机活跃 provider 为准。
            model_provider: Some(self.active_provider.clone()),
            approval_policy: Some(AskForApproval::Never.into()),
            sandbox: Some(sandbox.into()),
            ..Default::default()
        };
        let request = ClientRequest::ThreadResume {
            request_id: self.next_request_id(),
            params,
        };
        self.handle
            .request_typed(request)
            .await
            .map_err(|e| anyhow::anyhow!("thread/resume 失败: {e}"))
    }

    /// model/effort（DESIGN-ChatModelSelector §9.2 批次 5 远程 headless 透传）：
    /// schedule.model_override 解析出的 per-turn 覆盖；providerIndex 远程无效
    /// （多上游路由由 adapter 按模型名承担，§9.3 层 2）。
    #[allow(clippy::too_many_arguments)]
    pub async fn turn_start(
        &self,
        thread_id: &str,
        prompt: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<TurnStartResponse> {
        let params = TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            model: model.map(String::from),
            effort: effort.and_then(parse_remote_effort),
            ..Default::default()
        };
        let request = ClientRequest::TurnStart {
            request_id: self.next_request_id(),
            params,
        };
        self.handle
            .request_typed(request)
            .await
            .map_err(|e| anyhow::anyhow!("turn/start 失败: {e}"))
    }

    pub async fn turn_interrupt(&self, thread_id: &str, turn_id: &str) -> Result<()> {
        let request = ClientRequest::TurnInterrupt {
            request_id: self.next_request_id(),
            params: TurnInterruptParams {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
            },
        };
        self.handle
            .request_typed::<codex_app_server_protocol::TurnInterruptResponse>(request)
            .await
            .map_err(|e| anyhow::anyhow!("turn/interrupt 失败: {e}"))?;
        Ok(())
    }

    pub fn request_shutdown(&self) {
        self.shutdown_token.cancel();
    }
}

fn route_notification(routes: &DashMap<String, mpsc::Sender<RoutedEvent>>, n: ServerNotification) {
    let event = match &n {
        ServerNotification::TurnStarted(v) => RoutedEvent::TurnStarted {
            thread_id: v.thread_id.clone(),
            turn_id: v.turn.id.clone(),
        },
        ServerNotification::TurnCompleted(v) => {
            let status = format!("{:?}", v.turn.status);
            let turn_json = serde_json::to_value(&v.turn).unwrap_or_default();
            RoutedEvent::TurnCompleted {
                thread_id: v.thread_id.clone(),
                turn_id: v.turn.id.clone(),
                status,
                turn_json,
            }
        }
        ServerNotification::ItemCompleted(v) => match &v.item {
            ThreadItem::AgentMessage { text, .. } => RoutedEvent::AgentMessage {
                thread_id: v.thread_id.clone(),
                text: text.clone(),
            },
            // §4.9 证据源四类（v2.52 F2）：整件透传，executor 侧映射
            ThreadItem::CommandExecution { .. }
            | ThreadItem::FileChange { .. }
            | ThreadItem::McpToolCall { .. }
            | ThreadItem::WebSearch(_) => RoutedEvent::ItemCompleted {
                thread_id: v.thread_id.clone(),
                item: v.item.clone(),
            },
            _ => return,
        },
        _ => return,
    };
    let thread_id = event.thread_id().to_owned();
    if let Some(tx) = routes.get(thread_id.as_str()) {
        // channel 满时丢弃并告警（任务事件循环将等待至超时兜底；无日志则
        // 丢事件无任何痕迹，TurnCompleted 丢失会误判 timeout）
        if let Err(err) = tx.try_send(event) {
            tracing::warn!(thread.id = %thread_id, error = %err, "事件路由 channel 满，丢弃事件");
        }
    }
}

impl RoutedEvent {
    pub fn thread_id(&self) -> &str {
        match self {
            Self::TurnStarted { thread_id, .. }
            | Self::TurnCompleted { thread_id, .. }
            | Self::AgentMessage { thread_id, .. }
            | Self::ItemCompleted { thread_id, .. } => thread_id,
        }
    }
}
/// 交接 rollout 导入（§5.6.1 v2.46）：把上传的中转文件复制到
/// CODEX_HOME/sessions/YYYY/MM/DD/rollout-<时间戳>-<thread_id>.jsonl
/// 规范路径（幂等——目标已存在直接复用，不覆盖）。返回导入后路径，
/// 作为 thread/resume 的 path。
///
/// 缺失此步时 paginated rollout 的 path resume 会被 codex 按 meta 里的
/// thread id 反查本机存储（read_thread → resolve：state_db 无记录后扫
/// sessions 目录）而失败（no rollout found）。
async fn import_handoff_rollout(src: &str) -> Result<std::path::PathBuf> {
    use tokio::io::AsyncBufReadExt;

    let src_path = std::path::Path::new(src);
    let f = tokio::fs::File::open(src_path)
        .await
        .with_context(|| format!("打开交接 rollout 失败: {src}"))?;
    let mut reader = tokio::io::BufReader::new(f);
    let mut first_line = String::new();
    reader
        .read_line(&mut first_line)
        .await
        .with_context(|| format!("读取交接 rollout 首行失败: {src}"))?;
    let meta: serde_json::Value = serde_json::from_str(first_line.trim())
        .with_context(|| format!("交接 rollout session_meta 解析失败: {src}"))?;
    let payload = meta
        .get("payload")
        .ok_or_else(|| anyhow::anyhow!("交接 rollout 缺少 payload: {src}"))?;
    let thread_id = payload
        .get("id")
        .or_else(|| payload.get("session_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("交接 rollout 缺少 thread id: {src}"))?;
    let ts = payload
        .get("timestamp")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("交接 rollout 缺少 timestamp: {src}"))?;
    let dt = chrono::DateTime::parse_from_rfc3339(ts)
        .with_context(|| format!("交接 rollout timestamp 解析失败: {ts}"))?;

    let codex_home = find_codex_home().context("定位 CODEX_HOME 失败")?;
    let dir = codex_home
        .join("sessions")
        .join(dt.format("%Y").to_string())
        .join(dt.format("%m").to_string())
        .join(dt.format("%d").to_string());
    let name = format!(
        "rollout-{}-{thread_id}.jsonl",
        dt.format("%Y-%m-%dT%H-%M-%S")
    );
    let dst = dir.join(&name);
    if tokio::fs::try_exists(&dst).await.unwrap_or(false) {
        tracing::info!(path = %dst.display(), "交接 rollout 已导入（复用）");
        return Ok(dst.to_path_buf());
    }
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("创建 sessions 目录失败: {}", dir.display()))?;
    tokio::fs::copy(src_path, &dst)
        .await
        .with_context(|| format!("复制交接 rollout 失败: {src} → {}", dst.display()))?;
    tracing::info!(path = %dst.display(), "交接 rollout 导入完成");
    Ok(dst.to_path_buf())
}
/// 远程 effort 档位解析（§11.5：low/high/xhigh 白名单；无效返回 None 丢弃，
/// headless 无人值守不中断执行）
fn parse_remote_effort(s: &str) -> Option<ReasoningEffort> {
    match s {
        "low" => Some(ReasoningEffort::Low),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::XHigh),
        _ => None,
    }
}
