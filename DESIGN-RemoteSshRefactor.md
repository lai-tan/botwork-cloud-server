# 远程 SSH 模式重构设计文档

> **版本**：v2.15.0  
> **日期**：2026-10-08  
> **作者**：Jehu Shaw  
> **状态**：✅ 评审通过（九轮评审 + openclaw 协议实测确认）+ v2.10.0 远程 codex-rs 编译部署方案  
> **关联文档**：PRD-BotWorkWindows.md v1.0.1、HLD-BotWorkWindows.md v1.0.1、LLD-BotWorkWindows.md v1.0.1、SSH_REMOTE_AGENT_DESIGN.md

---

## 1. 背景与问题分析

### 1.1 当前架构

bot-work-windows 采用**非对称双模设计**：

- **本地模式**（`LocalAgentExecutor`）：codex-rs 库嵌入，Rust API 直接调用，性能优先
- **远程模式**（`SshAgentExecutor`）：SSH + `codex exec --jsonl`，CLI 子进程方式

本地模式已验证可用且性能最优，**本次重构不涉及本地模式**。

### 1.2 远程模式的致命问题

经源码分析确认，当前远程 SSH 模式存在**三个 P0 级致命 bug**，实际上无法正常工作：

| # | 问题 | 位置 | 根因 |
|---|------|------|------|
| P0-1 | `--jsonl` 参数不存在 | `ssh/agent.rs:34` | codex CLI 只认 `--json`（别名 `--experimental-json`），无 `--jsonl`；openclaw 用 `openclaw agent exec "msg" --json`，亦无 `--jsonl` |
| P0-2 | JSONL 格式不匹配 | `jsonl.rs` 全文 | `jsonl.rs` 期望扁平格式，codex 输出嵌套 `ThreadEvent` |
| P0-3 | 审批交互失效 | `jsonl.rs:63` | 不解析 `approval_request`；且 `codex exec --json` **自动拒绝所有审批** |

### 1.3 已有的策略模式基础

项目**已实现**远程 agent 生命周期管理策略：

- `RemoteAgentType` 枚举：`Codex` | `OpenClaw` | `ClaudeCode`（`model/mod.rs:33`）
- `RemoteAgentStrategy` trait：`check_installed` / `install` / `uninstall`（`remote_agent.rs:10`）
- 三个策略实现：`CodexStrategy`、`OpenClawStrategy`、`ClaudeCodeStrategy`

**但缺少通信协议适配层** — 不同远端的协议差异未抽象，当前硬编码 `codex exec --jsonl`。

### 1.4 三种远端的能力差异

| 能力 | codex | claude code | openclaw |
|------|-------|-------------|----------|
| **服务模式启动** | `codex app-server --listen stdio://` | `claude mcp serve` 或 `claude --input-format json --output-format json` | `openclaw --server --json-rpc`（预期） |
| **协议** | JSON-RPC 变体（省略 jsonrpc 字段） | MCP JSON-RPC 2.0 或 JSONL | 自定义 JSON 协议 |
| **审批交互** | 完整 ServerRequest | `--permission-mode` + JSON 事件 | 预期支持 |
| **会话管理** | thread/start, resume, fork | resume, fork, --session-id | 未知 |
| **配置传递** | `-c key=value` + RPC 参数 | 环境变量 + `--model` 等 | `openclaw config set` |
| **安装方式** | cargo build | `npm install -g @anthropic-ai/claude-code` | `curl -fsSL https://openclaw.ai/install.sh \| bash` |

---

## 2. 设计目标

| 目标 | 说明 |
|------|------|
| **G1 修复远程模式** | 解决三个 P0 问题，使 SSH 远程模式可用 |
| **G2 策略模式** | 通过策略模式支持 codex / openclaw / claude code 三种远端 |
| **G3 审批交互** | 远程模式下用户可审批/拒绝命令执行和文件变更 |
| **G4 配置传递** | model、api_key、base_url 等配置可传递给远端 |
| **G5 跨平台** | 远端核心可独立部署在 Linux/Windows 服务器上 |
| **G6 不影响本地** | 本地库嵌入模式完全不变 |
| **G7 能力协商** | 不同远端能力差异通过 capability negotiation 处理 |
| **G8 多 turn 对话** | 长连接复用，支持多轮对话而不重启远端进程 |

---

## 3. 方案选择

### 3.1 通信方案对比

| 方案 | 描述 | 审批 | 可插拔 | 多 turn |
|------|------|:---:|:---:|:---:|
| A. 修复 exec --json | 修正参数 + 重写解析器 | 不支持 | 差 | 不支持 |
| **B. 服务模式 stdio（长连接）** | **SSH + 远端服务模式 --listen stdio，保持长连接** | **支持** | **好** | **支持** |
| C. WebSocket + 隧道 | SSH 端口转发 + WebSocket | 支持 | 好 | 支持 |

### 3.2 选择方案 B：服务模式 stdio over SSH（长连接）

**理由**：

1. **审批完整**：服务模式支持完整的审批交互
2. **多 turn 支持**：长连接复用 SSH channel 和 thread_id，避免每条消息重启远端进程
3. **生命周期简单**：SSH 断开即远端退出
4. **可插拔**：通过协议适配层支持不同远端
5. **跨平台**：三种远端均支持 Linux/Windows

---

## 4. 架构设计

### 4.1 双层策略架构

```
┌──────────────────────────────────────────────────────────┐
│  UI (Tauri + React)                                      │
├──────────────────────────────────────────────────────────┤
│  AgentExecutor trait (不变)                              │
│  ├─ LocalAgentExecutor     (库嵌入, 不变)                 │
│  └─ SshAgentExecutor       (重写)                        │
├──────────────────────────────────────────────────────────┤
│  第1层: RemoteAgentStrategy (已有, 生命周期管理)          │
│  ├─ CodexStrategy          (check_installed/install/...)  │
│  ├─ OpenClawStrategy       (check_installed/install/...)  │
│  └─ ClaudeCodeStrategy     (check_installed/install/...)  │
├──────────────────────────────────────────────────────────┤
│  第2层: RemoteProtocolAdapter (新增, 通信协议适配)        │
│  ├─ CodexAppServerAdapter  (JSON-RPC over stdio)          │
│  ├─ OpenClawAdapter        (自定义 JSON 协议)             │
│  └─ ClaudeCodeAdapter      (MCP JSON-RPC 或 JSONL)        │
├──────────────────────────────────────────────────────────┤
│  SSH Transport (russh, 已有)                             │
│  ├─ channel stdin  → 协议请求                            │
│  └─ channel stdout → 协议响应/通知                       │
└──────────────────────────────────────────────────────────┘
```

### 4.2 两层策略的职责划分

| 层 | trait | 职责 | 已有? |
|----|-------|------|:---:|
| 第1层 | `RemoteAgentStrategy` | 生命周期：检查安装、安装、卸载 | ✅ |
| 第2层 | `RemoteProtocolAdapter` | 通信：构造命令、解析消息、审批响应、中断 | ❌ 新增 |

### 4.3 远程运行时（长连接模式）

```
远程机器 (Linux/Windows)
└── {start_command}  (由 SSH exec 启动, 长期运行)
    ├── stdin  ← 协议请求 (initialize, thread/start, turn/start, 审批响应, ...)
    └── stdout → 协议响应/通知 (事件流, 审批请求, 错误, ...)

连接生命周期:
  首次 execute():  SSH exec 启动远端 → initialize → thread/start → turn/start → 读取事件流
  后续 execute():  复用 SSH channel + thread_id → turn/start → 读取事件流
  close():         关闭 SSH channel → 远端进程退出

各远端的 start_command:
  codex:       codex app-server --listen stdio:// -c model="..." -c ...
  openclaw:    openclaw --server --json-rpc --model "..."
  claude-code: claude mcp serve  或  claude --input-format json --output-format json --model "..."
```

### 4.4 模块结构

```
src-tauri/src/
├── executor/
│   ├── mod.rs                    # AgentExecutor trait (不变)
│   ├── local/                    # 本地模式 (不变)
│   ├── ssh/
│   │   ├── agent.rs              # 重写: SshAgentExecutor (长连接)
│   │   ├── file.rs               # 不变 (SFTP)
│   │   └── desktop.rs            # 不变 (VNC)
│   ├── remote_protocol/          # 新增: 协议适配层
│   │   ├── mod.rs                # RemoteProtocolAdapter trait + RemoteMessage + RemoteCapabilities + SshChannelWriter/Reader/Pair + 工厂
│   │   ├── codex_adapter.rs      # CodexAppServerAdapter
│   │   ├── openclaw_adapter.rs   # OpenClawAdapter
│   │   ├── claude_code_adapter.rs# ClaudeCodeAdapter
│   │   └── json_rpc.rs           # JSON-RPC 消息编解码
│   └── jsonl.rs                  # 废弃
├── domain/
│   ├── model/mod.rs              # Agent + RemoteAgentType (不变)
│   └── strategy/
│       ├── remote_agent.rs       # RemoteAgentStrategy (不变, 已有)
│       └── factory.rs            # 修改: 创建 adapter + strategy 并传入 executor
└── ssh/
    └── manager.rs                # 扩展: exec_bidirectional (替代 exec_stream)
```

---

## 5. 协议适配层设计

### 5.1 SSH 通道抽象

> **设计要点（解决 N1+N2+R1）**：russh 的 `Channel<Msg>` 是单一对象，同时拥有读和写能力，无法拆分为两个各自独立持有的 owned 对象。若用 `Arc<Mutex<Channel>>` 共享，`read_line` 阻塞期间持有锁，中断/审批写入会被阻塞（死锁）。
>
> **方案**：用 **reader task + mpsc channel** 模式，并将通道拆分为独立的 **Writer** 和 **Reader**：
> - 一个 dedicated reader task 持续从 Channel stdout 读取字节，按 `\n` 分割为完整行，推送到 `mpsc::Sender`
> - `SshChannelWriter` 持有 `Arc<Channel<Msg>>`，`write_line()` 是 `&self` 方法，可与 reader task 并发
> - `SshChannelReader` 持有 `mpsc::Receiver`，`read_line()` 从 mpsc 接收，不涉及 Channel 锁
> - **关键**：Writer 和 Reader 各自独立，无共享 Mutex。`execute()` 持有 `channel_reader` 锁等待远端输出时，`interrupt()` 可同时获取 `channel_writer` 锁发送中断命令，无死锁

```rust
/// SSH 通道写入端 — 独立持有 Arc<Channel>，无锁
/// 可随时写入 stdin，与 reader task 并发执行
pub struct SshChannelWriter {
    channel: Arc<Channel<Msg>>,
}

impl SshChannelWriter {
    /// 写入一行（自动追加 \n）
    /// &self 方法，可与 reader task 并发调用
    pub async fn write_line(&self, line: &str) -> AppResult<()> {
        let data = format!("{}\n", line);
        self.channel.data(data.as_bytes()).await?;
        Ok(())
    }

    /// 关闭 channel（发送 EOF）
    pub async fn close(&self) -> AppResult<()> {
        self.channel.close().await?;
        Ok(())
    }
}

/// SSH 通道读取端 — 独立持有 mpsc::Receiver，无锁
/// 从 reader task 接收完整行，不涉及 Channel 锁
pub struct SshChannelReader {
    line_rx: mpsc::Receiver<AppResult<String>>,
    /// reader task handle（close 时 abort）
    reader_task: Option<JoinHandle<()>>,
}

impl SshChannelReader {
    /// 读取一行（从 reader task 的 mpsc 接收）
    /// 不持有任何 Channel 锁，不阻塞写入端
    pub async fn read_line(&mut self) -> AppResult<String> {
        match self.line_rx.recv().await {
            Some(Ok(line)) => Ok(line),
            Some(Err(e)) => Err(e),
            None => Err(AppError::SshChannelClosed),
        }
    }

    /// 停止 reader task
    pub fn abort(&mut self) {
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
    }
}

/// 由 exec_bidirectional 返回的写入端和读取端
/// 两者独立持有各自资源，无共享 Mutex
pub struct SshChannelPair {
    pub writer: SshChannelWriter,
    pub reader: SshChannelReader,
}

/// reader task 内部逻辑（由 exec_bidirectional spawn）
async fn reader_task(
    channel: Arc<Channel<Msg>>,
    line_tx: mpsc::Sender<AppResult<String>>,
) {
    let mut buffer = Vec::<u8>::new();
    loop {
        // 从 channel stdout 读取字节（channel 的 AsyncRead 实现）
        let mut buf = [0u8; 4096];
        match channel.read(&mut buf).await {
            Ok(0) => {
                // EOF：推送缓冲区中剩余数据（如果有）
                if !buffer.is_empty() {
                    let line = String::from_utf8_lossy(&buffer).to_string();
                    let _ = line_tx.send(Ok(line)).await;
                }
                let _ = line_tx.send(Err(AppError::SshChannelClosed)).await;
                break;
            }
            Ok(n) => {
                buffer.extend_from_slice(&buf[..n]);
                // 按 \n 分割为完整行
                while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let line_str = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]).to_string();
                    if line_tx.send(Ok(line_str)).await.is_err() {
                        return; // receiver dropped, exit task
                    }
                }
            }
            Err(e) => {
                let _ = line_tx.send(Err(AppError::from(e))).await;
                break;
            }
        }
    }
}
```

> **stderr 处理（解决 R25）**：`channel.read()` 仅读 stdout，远端 stderr 输出（错误信息、日志）不会被 reader_task 捕获。实现时额外 spawn 一个 stderr reader task 读取 `channel.stderr()`，将 stderr 内容通过 `tracing::warn!` 记录日志（不推送到 mpsc，避免与协议消息混淆）。远端崩溃时 stderr 错误信息可在日志中查看。

### 5.2 RemoteProtocolAdapter trait

> **设计要点**：trait 方法签名使用 `SshChannelWriter` / `SshChannelReader`（§5.1）而非单一 handle。`initialize` 和 `create_thread` 需要读写（`&Writer, &mut Reader`），`start_turn` 只需写（`&Writer`）。拆分 Writer/Reader 使 `interrupt()` 可在 `execute()` 读取期间并发写入。

```rust
/// 远端通信协议适配器 — 策略模式核心接口
///
/// 每种远端 (codex / openclaw / claude code) 实现此 trait，
/// 负责将统一的 AgentExecutor 语义翻译为远端特有的协议格式。
#[async_trait]
pub trait RemoteProtocolAdapter: Send + Sync {
    // ── 命令构造 ──

    /// 构造远端启动命令（含配置传递）
    /// multi_turn=true 时使用（长连接模式，消息通过 stdin 发送）
    fn build_start_command(&self, agent: &Agent) -> String;

    /// 构造 one-shot 启动命令（multi_turn=false 时使用）
    /// 消息嵌入命令本身，进程执行完即退出，无长连接复用
    /// multi_turn=true 的远端可返回空字符串（不会调用）
    /// attachments 嵌入命令参数（如 claude --attach file_path）；不支持的可忽略
    fn build_one_shot_command(&self, agent: &Agent, message: &str, attachments: &[AttachmentData]) -> String;

    /// 构造环境变量（如 ANTHROPIC_API_KEY、CODEX_API_KEY 等）
    fn build_env_vars(&self, agent: &Agent) -> HashMap<String, String>;

    // ── 连接生命周期 ──

    /// 初始化握手（如 codex 的 initialize 请求）
    /// 发送 Request（通过 writer），等待匹配 id 的 Response（通过 reader）
    /// 返回远端实际协商的能力集（可能覆盖静态 capabilities() 声明）
    /// writer 用 &self（write_line 是 &self 方法），reader 用 &mut（read_line 需 &mut）
    async fn initialize(
        &self,
        writer: &SshChannelWriter,
        reader: &mut SshChannelReader,
    ) -> AppResult<RemoteCapabilities>;

    /// 创建线程（如 codex 的 thread/start Request）
    /// 发送 Request（通过 writer），等待匹配 id 的 Response（通过 reader，从中提取 thread_id）
    /// 注意：thread/started 是独立通知，不用于获取 thread_id
    /// 返回 thread_id
    async fn create_thread(
        &self,
        agent: &Agent,
        writer: &SshChannelWriter,
        reader: &mut SshChannelReader,
    ) -> AppResult<String>;

    /// 发起 turn（如 codex 的 turn/start）
    /// 复用已有 thread_id，只需写入
    /// attachments 传递给远端（codex 通过 turn/start params 的 attachments 字段；
    /// claude code MCP 通过 tools/call 参数；不支持 attachments 的远端可忽略）
    async fn start_turn(
        &self,
        thread_id: &str,
        message: &str,
        attachments: &[AttachmentData],
        writer: &SshChannelWriter,
    ) -> AppResult<()>;

    /// 中断当前 turn
    /// 返回 None 表示此远端不支持中断（调用方应静默忽略）
    fn format_interrupt(&self, thread_id: &str) -> Option<String>;

    // ── 消息解析 ──

    /// 解析远端输出的一行 → RemoteMessage
    fn parse_line(&self, line: &str) -> AppResult<RemoteMessage>;

    // ── 审批 ──

    /// 构造审批响应
    fn format_approval_response(
        &self,
        request_id: &str,
        decision: &ApprovalDecision,
    ) -> String;

    // ── 上下文压缩 ──

    /// 构造 compact 请求（上下文压缩）
    /// 返回 None 表示此远端不支持 compact
    /// 调用方在发送前应检查 capabilities.compact
    fn format_compact(&self, thread_id: &str) -> Option<String>;

    // ── 能力 ──

    /// 返回此远端的静态能力声明
    /// initialize() 返回的实际协商结果可能覆盖此值
    fn capabilities(&self) -> RemoteCapabilities;
}
```

### 5.3 能力协商

不同远端能力不同，通过 `RemoteCapabilities` 声明：

```rust
#[derive(Debug, Clone)]
pub struct RemoteCapabilities {
    /// 是否支持审批交互
    pub approval: bool,
    /// 是否支持会话恢复
    pub session_resume: bool,
    /// 是否支持会话分叉
    pub session_fork: bool,
    /// 是否支持流式输出（item/updated 事件）
    pub streaming: bool,
    /// 是否支持 token 使用量报告
    pub usage_report: bool,
    /// 是否支持中断
    pub interrupt: bool,
    /// 是否支持多 turn（非一次性）
    pub multi_turn: bool,
    /// 是否支持 compact（上下文压缩）
    pub compact: bool,
    /// 协议类型
    pub protocol: ProtocolType,
}

#[derive(Debug, Clone)]
pub enum ProtocolType {
    JsonRpc,     // codex app-server
    McpJsonRpc,  // claude code mcp serve
    Jsonl,       // claude code --output-format json
    Custom,      // openclaw
}
```

**`capabilities()` 与 `initialize()` 的关系**：
- `capabilities()` 返回**静态声明**（编译时已知的能力）
- `initialize()` 返回**实际协商结果**（运行时远端返回的能力，可能覆盖静态值）
- `SshAgentExecutor` 使用 `initialize()` 的返回值作为运行时能力集

各远端能力声明：

| 能力 | codex | claude code | openclaw |
|------|:---:|:---:|:---:|
| approval | ✅ | ✅ | ❌ |
| session_resume | ✅ | ✅ | ❌ |
| session_fork | ✅ | ✅ | ❌ |
| streaming | ✅ | ✅ | ❌ |
| usage_report | ✅ | ✅ | ❌ |
| interrupt | ✅ | ✅ | ❌ |
| multi_turn | ✅ | ✅/❌¹ | ❌ |
| compact | ✅ | ❓ | ❌ |
| protocol | JsonRpc | McpJsonRpc | Custom |

> ¹ `multi_turn` 取决于实际模式：MCP 模式 `true`，JSONL/fallback 模式 `false`。运行时由 `initialize()` 协商结果或 `capabilities()` 静态声明决定。

### 5.4 消息类型

> **设计要点（解决 N3）**：`initialize` 和 `create_thread` 发送 JSON-RPC **Request**（含 `id`），需要等待对应的 **Response**（匹配 `id`，含 `result`）。但原 `RemoteMessage` 无 `Response` 变体，无法表达响应。新增 `Response` 变体供 adapter 内部 `initialize` / `create_thread` 使用；execute 主循环不处理 `Response`（响应已在 adapter 内部消费）。

```rust
/// 远端消息的统一表示
pub enum RemoteMessage {
    /// JSON-RPC 响应（匹配某个请求的 id，含 result 或 error）
    /// 供 adapter 的 initialize() / create_thread() 内部等待请求响应
    /// execute 主循环收到 Response 时按 Ignored 处理（响应已在 adapter 内消费）
    Response {
        id: String,
        /// 成功时的 result（None 表示 error）
        result: Option<serde_json::Value>,
        /// 错误时的 message（None 表示成功）
        error: Option<String>,
    },
    /// 转换为 MessageSegment 推前端
    Segment(MessageSegment),
    /// 审批请求（需用户决策）
    ApprovalRequest {
        id: String,
        kind: String,
        content: String,
        available_decisions: Vec<String>,
    },
    /// 线程已启动通知（thread/started 通知，非 thread/start 响应）
    ThreadStarted { thread_id: String },
    /// Turn 已完成（含 usage）
    TurnCompleted { usage: Option<UsageInfo> },
    /// 错误
    Error { message: String },
    /// 忽略（心跳、调试等）
    Ignored,
}
```

**类型一致性说明**：`RemoteMessage::ApprovalRequest.available_decisions` 使用 `Vec<String>` 与 `MessageSegment::ApprovalRequest.available_decisions`（`mod.rs:33`）保持一致，无需类型转换。

### 5.5 审批决策映射

`ApprovalDecision` 枚举（`mod.rs:78`）到各远端协议字符串的映射：

| ApprovalDecision | codex JSON-RPC | claude code | openclaw |
|------------------|:---:|:---:|:---:|
| `Allow` | `"accept"` | `"allow"` | `"allow"` |
| `Deny` | `"decline"` | `"deny"` | `"deny"` |
| `AlwaysAllow` | `"accept_for_session"` | `"always_allow"` | `"always_allow"` |
| `AlwaysAllowGlobal` | `"accept_for_session"` | `"always_allow"` | `"always_allow"` |

> 注：codex 的 `CommandExecutionApprovalDecision` 无 `AlwaysAllowGlobal`，映射到 `AcceptForSession`。

---

## 6. 各远端适配器设计

### 6.1 CodexAppServerAdapter

**启动命令**：
```bash
codex app-server --listen stdio:// -c model="agnes-3.0-flash" -c model_providers.openai.base_url="https://..."
```

**协议**：JSON-RPC 变体（省略 `jsonrpc` 字段），NDJSON 传输

**通信流程**：
```
initialize (clientInfo: {name, version}) → thread/start (model, config) → turn/start (threadId, input: [{type:"text",text:"..."}])
  ← thread/started ← turn/started ← item/started ← item/agentMessage/delta ← item/completed
  ← item/commandExecution/requestApproval (ServerRequest)
  → {id, result: {decision: "accept"}}
  ← turn/completed (usage)
```

**thread 生命周期（解决 N5）**：

> **关键区分**：`thread/start` 是 JSON-RPC **Request**（含 `id`，期望 Response），`thread/started` 是独立 **Notification**（无 `id`，方法名 = `thread/started`）。`create_thread` 从 **Response** 中提取 `thread_id`，不从 `thread/started` 通知中提取。

- `create_thread` 实现逻辑：
  1. 生成请求 id（如 `"thread-start-1"`），发送 `{"id":"thread-start-1","method":"thread/start","params":{"model":"...","config":{...}}}`
  2. 循环 `reader.read_line()` → `parse_line()`，直到收到 `RemoteMessage::Response { id: "thread-start-1", result, .. }`
  3. 从 `result` 中提取 `thread_id`（codex 的 `thread/start` Response 的 `result.thread.id` 包含 thread ID）
  4. 收到 `thread/started` 通知（`RemoteMessage::ThreadStarted`）时忽略（thread_id 已从 Response 获取）
  5. 返回 `thread_id`
- `start_turn`：发送 `turn/start`（复用 thread_id + 新 input）
- 多 turn 复用：同一 SSH channel 上，首次 `create_thread` 后存 thread_id，后续 `execute()` 只调 `start_turn`

**通知事件映射**：

> **实测确认（v0.160.1）**：codex 无 `item/updated` 通知，改为专用 delta 通知（`item/agentMessage/delta`、`item/commandExecution/outputDelta`、`item/reasoning/textDelta` 等）。

| JSON-RPC 通知 | RemoteMessage |
|---------------|---------------|
| `thread/started` | `ThreadStarted { thread_id }` |
| `turn/started` | `Segment(Thinking { content: "", elapsed_ms: Some(0) })` |
| `item/started` + `command_execution` | `Segment(ToolCall { tool: "exec", args: command, result: "", status: "running" })` |
| `item/agentMessage/delta` | `Segment(Text { content: delta })` |
| `item/reasoning/textDelta` | `Segment(Thinking { content: delta, elapsed_ms: None })` |
| `item/commandExecution/outputDelta` | `Segment(CommandBlock { command: "", output: delta })` |
| `item/completed` + `agent_message` | `Segment(Text { content: text })` |
| `item/completed` + `reasoning` | `Segment(Thinking { content: text, elapsed_ms: None })` |
| `item/completed` + `command_execution` | `Segment(CommandBlock { command, output: aggregated_output })` |
| `item/completed` + `file_change` | `Segment(DiffBlock { file_path, content })` |
| `item/completed` + `mcp_tool_call` | `Segment(ToolCall { tool, args, result, status })` |
| `turn/completed` | `TurnCompleted { usage }` |
| `turn/failed` / `error` | `Error { message }` |
| `warning` | `Segment(Warning { message })` |
| `item/commandExecution/requestApproval` | `ApprovalRequest { id, kind: "exec", content: command, available_decisions }` |
| `item/fileChange/requestApproval` | `ApprovalRequest { id, kind: "patch", content, available_decisions }` |

**审批响应格式**：
```json
{"id":"sr-1","result":{"decision":"accept"}}
```

**配置传递**：

| 配置项 | 方式 |
|--------|------|
| model | `thread/start` 参数 `{"model":"..."}` |
| api_key | 环境变量 `CODEX_API_KEY=xxx` |
| base_url | 命令行 `-c model_providers.openai.base_url="..."` |
| approval_policy | `thread/start` 参数 `{"approval_policy":"on_request"}` |
| sandbox | `thread/start` 参数 `{"sandbox":"none"}` |
| cwd | `thread/start` 参数 `{"cwd":"..."}` |

### 6.2 ClaudeCodeAdapter

**模式选择策略**：
- **优先 MCP 模式**（`claude mcp serve`）：双向 JSON-RPC，支持审批、多 turn
- **降级 JSONL 模式**（`claude --output-format json`）：单向输出，不支持审批和多 turn
- 降级条件：MCP 模式启动失败（`claude mcp serve` 不存在或返回错误）

**启动命令**：

MCP 模式（推荐）：
```bash
claude mcp serve --model "claude-sonnet-4-20250514"
```

JSONL 模式（降级）：
```bash
claude -p "message" --output-format json --input-format json --model "..." --permission-mode default
```

**协议**：
- MCP 模式：标准 JSON-RPC 2.0（含 `jsonrpc` 字段），NDJSON 传输
- JSONL 模式：JSONL 事件流（单向，`multi_turn = false`）

**create_thread 语义（解决 N6）**：

> **关键差异**：MCP 协议无 `thread/start` 概念。`claude mcp serve` 提供 tools/resources 接口，不管理对话线程。需通过映射或虚拟 thread_id 适配。

- **MCP 模式**：`create_thread` 不发送任何请求，直接返回虚拟 thread_id（`"claude-mcp-default"`）。对话上下文由 claude code 内部维护，通过 `--session-id` 参数或 MCP 会话标识关联。`start_turn` 发送 MCP `tools/call` 请求（如 `claude_chat` tool），每次 turn 传入完整消息。
- **JSONL 模式**：`build_one_shot_command` 构造完整命令 `claude -p "message" --output-format json --session-id <uuid> ...`，消息嵌入命令本身。每次 `execute()` 走 one-shot 流程（新 SSH exec，`multi_turn = false`，无长连接复用）。不调用 `initialize` / `create_thread` / `start_turn`。
- **能力反映**：MCP 模式 `multi_turn = true`；JSONL 模式 `multi_turn = false`。`capabilities()` 根据实际模式返回。

**配置传递**：

| 配置项 | 方式 |
|--------|------|
| model | `--model` 参数 |
| api_key | 环境变量 `ANTHROPIC_API_KEY=xxx` |
| base_url | 环境变量 `ANTHROPIC_BASE_URL=xxx` |
| permission_mode | `--permission-mode` 参数 |
| allowed_tools | `--allowedTools` 参数 |

**审批**：通过 `--permission-mode default` 启用审批；JSON 事件中包含 `tool_use_permission_request` 事件

### 6.3 OpenClawAdapter

> **协议已确认（2026-10-07 远程实测 v2026.9.8）**：openclaw 用 `openclaw agent exec` 子命令执行 one-shot agent turn，`--json` 产生稳定 JSON envelope（一次性 JSON 输出到 stdout，非 NDJSON 流式）。无 `--server` / `--json-rpc` / `--jsonl` 参数。

**启动命令**（one-shot，`multi_turn=false`）：
```bash
openclaw agent exec "message" --json --model "openai/gpt-4o-mini" --cwd "/path/to/workdir" --session-id "..."
```

**协议**：JSON envelope（一次性 JSON 输出到 stdout，非流式 NDJSON）

**JSON envelope 格式**（`openclaw agent exec --json` 输出）：
```json
{
  "ok": true,
  "status": "ok",
  "final": "agent reply text",
  "payloads": [...],
  "model": "openai/gpt-4o-mini",
  "provider": "openai",
  "sessionId": "uuid",
  "error": null
}
```

错误时：
```json
{
  "ok": false,
  "status": "error",
  "final": "",
  "payloads": [],
  "model": null,
  "provider": null,
  "sessionId": "uuid",
  "error": { "message": "...", "kind": "exception" }
}
```

**stderr 处理**：诊断日志（`[diagnostic]`、`[model-fallback/decision]` 等）输出到 stderr，不混入 stdout JSON。`reader_task` 仅读 stdout。

**`parse_line` 逻辑**：
- one-shot 模式下 stdout 是完整 JSON（可能单行或多行），`reader_task` 按 `\n` 分割推送
- `parse_line` 累积行直到收到完整 JSON envelope（`}` 闭合），然后解析为 `RemoteMessage`
- `ok=true` → `Segment(Text { content: final })` + `TurnCompleted { usage: None }`
- `ok=false` → `Error { message: error.message }`

**配置传递**：

| 配置项 | 方式 |
|--------|------|
| model | `--model "provider/model"` 参数 |
| api_key | 环境变量 `OPENAI_API_KEY=xxx` |
| cwd | `--cwd "..."` 参数 |
| session_id | `--session-id "..."` 参数 |
| thinking | `--thinking <level>` 参数 |

**`build_one_shot_command` 构造**：
```rust
fn build_one_shot_command(&self, agent: &Agent, message: &str, _attachments: &[AttachmentData]) -> String {
    let model = agent.model.as_deref().unwrap_or("openai/gpt-4o-mini");
    let cwd = agent.work_dir.as_deref().unwrap_or(".");
    format!(
        "openclaw agent exec {} --json --model \"{}\" --cwd \"{}\"",
        shell_escape(message), model, cwd
    )
}
```

**能力声明**：`multi_turn=false`、`protocol=Custom`、`approval=false`（exec 模式不支持审批交互）、`interrupt=false`、`streaming=false`（一次性输出，非流式）

**实现状态**：协议已确认，可直接实现。使用 one-shot 模式（`build_one_shot_command`），不走长连接流程（不调用 `initialize` / `create_thread` / `start_turn`）。

### 6.4 适配器工厂

```rust
pub fn create_protocol_adapter(agent: &Agent) -> Box<dyn RemoteProtocolAdapter> {
    let agent_type = agent.remote_agent_type
        .as_ref()
        .map(|t| t.as_str())
        .unwrap_or("codex");

    match agent_type {
        "codex" => Box::new(CodexAppServerAdapter::new()),
        "openclaw" => Box::new(OpenClawAdapter::new()),
        "claude-code" => Box::new(ClaudeCodeAdapter::new()),
        _ => Box::new(CodexAppServerAdapter::new()), // 默认
    }
}

pub fn create_remote_strategy(agent: &Agent) -> Box<dyn RemoteAgentStrategy> {
    let agent_type = agent.remote_agent_type
        .as_ref()
        .map(|t| t.as_str())
        .unwrap_or("codex");

    match agent_type {
        "codex" => Box::new(CodexStrategy),
        "openclaw" => Box::new(OpenClawStrategy),
        "claude-code" => Box::new(ClaudeCodeStrategy),
        _ => Box::new(CodexStrategy),
    }
}
```

---

## 7. SshAgentExecutor 重写

### 7.0 审批通道管理器

> **设计要点（解决 R37）**：现有 `ApprovalChannels` 是类型别名 `Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>`，无 `register()` / `cancel_all()` 方法。新增 `ApprovalChannelManager` 包装结构体提供清晰 API。

```rust
/// 审批通道管理器 — 封装 ApprovalChannels 提供 register / cancel_all / take 方法
pub struct ApprovalChannelManager {
    channels: Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>,
}

impl ApprovalChannelManager {
    pub fn new() -> Self {
        Self { channels: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// 从现有 ApprovalChannels 构造（兼容 factory 已有实例）
    pub fn from_channels(channels: ApprovalChannels) -> Self {
        Self { channels }
    }

    /// 注册审批请求的 oneshot::Sender，返回 oneshot::Receiver 供 execute 循环等待
    pub async fn register(&self, id: &str, tx: oneshot::Sender<ApprovalDecision>) {
        self.channels.lock().await.insert(id.to_string(), tx);
    }

    /// 取出并移除指定 id 的 Sender（供 submit_approval 使用）
    /// 返回 None 表示 id 不存在或已被 cancel_all 清除
    pub async fn take(&self, id: &str) -> Option<oneshot::Sender<ApprovalDecision>> {
        self.channels.lock().await.remove(id)
    }

    /// 取消所有待审批请求（close 时调用，使所有 rx.await 返回 Err）
    pub async fn cancel_all(&self) {
        self.channels.lock().await.clear();
    }
}
```

### 7.1 结构

> **设计要点**：`channel_writer` 和 `channel_reader` 分别用独立 `Mutex` 存储（§5.1 拆分设计）。`execute()` 持有 `channel_reader` 锁等待远端输出时，`interrupt()` 可同时获取 `channel_writer` 锁发送中断命令，无死锁（解决 R1）。

```rust
pub struct SshAgentExecutor {
    ssh_manager: Arc<SshManager>,
    agent: Agent,
    strategy: Box<dyn RemoteAgentStrategy>,    // 第1层: 生命周期
    adapter: Box<dyn RemoteProtocolAdapter>,   // 第2层: 通信协议
    approval_channels: ApprovalChannelManager,
    // 长连接状态 — writer 和 reader 分离，各自独立 Mutex
    channel_writer: Mutex<Option<SshChannelWriter>>,         // SSH 写入端
    channel_reader: Mutex<Option<SshChannelReader>>,         // SSH 读取端
    thread_id: Mutex<Option<String>>,                        // 远端 thread ID
    capabilities: Mutex<Option<RemoteCapabilities>>,         // 协商后的能力集
    // session_id ↔ thread_id 映射（解决 N8）
    // 当前设计：一个 executor 实例对应一个远端 thread（1:1）
    // session_id 参数用于本地日志关联，不用于多 session 路由
    // 若未来需支持多 session per executor，改为 HashMap<session_id, thread_id>
    turn_lock: Mutex<()>,                                     // 并发 turn 互斥锁
}
```

**构造函数签名变更**：

```rust
impl SshAgentExecutor {
    pub fn new(
        ssh_manager: Arc<SshManager>,
        agent: Agent,
        approval_channels: ApprovalChannelManager,
        strategy: Box<dyn RemoteAgentStrategy>,
        adapter: Box<dyn RemoteProtocolAdapter>,
    ) -> Self { ... }
}
```

`factory.rs` 对应改动：

```rust
// factory.rs create_executor()
ExecutorType::Ssh => {
    let strategy = create_remote_strategy(agent);
    let adapter = create_protocol_adapter(agent);
    Ok(Arc::new(SshAgentExecutor::new(
        ssh_manager,
        agent.clone(),
        ApprovalChannelManager::from_channels(approval_channels),
        strategy,
        adapter,
    ))))
}
```

### 7.2 execute 流程（长连接多 turn + one-shot 双模式）

> **设计要点**：
> - **N4**：`approval_rx` 从 `self.approval_channels` 获取。`ApprovalChannelManager` 内部维护 `HashMap<request_id, oneshot::Sender<ApprovalDecision>>`。发送 `ApprovalRequest` 到前端时，创建 `oneshot::channel()`，注册 sender，保存 receiver。execute 循环中 `rx.await` 等待前端通过 `submit_approval` 传递决策。
> - **N8**：`session_id` 是本地应用的会话标识，`thread_id` 是远端的线程标识。当前设计 1:1（一个 executor 实例 = 一个远端 thread），`session_id` 仅用于本地日志关联，不参与远端路由。
> - **R1**：`channel_reader` 和 `channel_writer` 是独立 Mutex。`read_line()` 持有 `channel_reader` 锁，不阻塞 `interrupt()` 获取 `channel_writer` 锁。
> - **R20**：setup 阶段（step 1d-1e initialize/create_thread）同时持有 `channel_writer` + `channel_reader` 锁，`interrupt()` / `close()` 会阻塞等待。这是 **非死锁**（setup 是一次性 I/O 操作，不等待锁），setup 完成后锁释放。setup 阶段无 turn 进行，interrupt 语义无意义，阻塞可接受。
> - **R3**：`multi_turn=true`（codex MCP、claude code MCP）使用长连接模式；`multi_turn=false`（claude code JSONL、openclaw fallback）使用 one-shot 模式（每次 execute 新 SSH exec，不复用 channel）。
> - **并发 turn 保护**：`turn_lock` 确保同一 executor 不允许并发 turn。
> - **attachments 传递**：长连接模式通过 `start_turn` 参数传递（codex `turn/start` params 的 `attachments` 字段）；one-shot 模式通过 `build_one_shot_command` 嵌入命令参数。不支持 attachments 的远端可忽略（adapter 内部丢弃）。

**模式判断**：`execute()` 入口检查 `self.capabilities` 中的 `multi_turn` 字段。首次调用时 capabilities 尚未协商，使用 `adapter.capabilities().multi_turn` 静态声明判断。

```rust
async fn execute(&self, session_id: &str, message: &str, attachments: &[AttachmentData], sender: mpsc::Sender<MessageSegment>) -> AppResult<()> {
    let conn_id = self.agent.connection_id.as_ref()?;

    // 0. 并发 turn 互斥
    let _turn_guard = self.turn_lock.lock().await;

    // 判断模式：优先用已协商的 capabilities，否则用静态声明
    let multi_turn = {
        let caps = self.capabilities.lock().await;
        caps.as_ref().map(|c| c.multi_turn)
            .unwrap_or_else(|| self.adapter.capabilities().multi_turn)
    };

    if multi_turn {
        self.execute_long_connection(session_id, message, attachments, &sender).await
    } else {
        self.execute_one_shot(session_id, message, attachments, &sender).await
    }
}
```

**长连接模式（multi_turn=true）**：

```rust
async fn execute_long_connection(&self, _session_id: &str, message: &str, attachments: &[AttachmentData], sender: &Sender<MessageSegment>) -> AppResult<()> {
    let conn_id = self.agent.connection_id.as_ref()?;

    // 1. 首次连接：检查安装 + 启动远端 + 握手 + 创建 thread
    let needs_connect = {
        let writer = self.channel_writer.lock().await;
        writer.is_none()
    };
    if needs_connect {
        // 1a. 检查远端是否安装（第1层策略）
        if !self.strategy.check_installed(&self.ssh_manager, conn_id).await? {
            self.strategy.install(&self.ssh_manager, conn_id).await?;
        }
        // 1b. 构造启动命令 + 环境变量（第2层适配器）
        let command = self.adapter.build_start_command(&self.agent);
        let env_vars = self.adapter.build_env_vars(&self.agent);
        // 1c. SSH exec 启动远端服务（长连接，返回 SshChannelPair）
        let pair = self.ssh_manager.exec_bidirectional(conn_id, &command, &env_vars).await?;
        *self.channel_writer.lock().await = Some(pair.writer);
        *self.channel_reader.lock().await = Some(pair.reader);
    }

    // 1d-1e. 首次握手 + 创建 thread（需要 writer + reader）
    // R29: initialize/create_thread 失败时清理 channel，使下次 execute 重建连接
    let setup_result: AppResult<()> = {
        let writer = self.channel_writer.lock().await;
        let mut reader = self.channel_reader.lock().await;
        if self.thread_id.lock().await.is_some() {
            Ok(()) // 已有 thread_id，跳过
        } else {
            // R49: close() 并发可将 channel 置 None，用 ok_or_else 替代 unwrap 防 panic
            let w = writer.as_ref()
                .ok_or_else(|| AppError::Executor("connection closed during setup".to_string()))?;
            let r = reader.as_mut()
                .ok_or_else(|| AppError::Executor("connection closed during setup".to_string()))?;
            // 1d-1e. 初始化握手 → 创建 thread
            // R41: 嵌套 match，Err(e) 作为块值返回（不用 return），使外层 if let Err 可清理 channel
            match self.adapter.initialize(w, r).await {
                Ok(caps) => {
                    *self.capabilities.lock().await = Some(caps);
                    match self.adapter.create_thread(&self.agent, w, r).await {
                        Ok(tid) => {
                            *self.thread_id.lock().await = Some(tid);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            }
        }
    };
    // R29: setup 失败时清理 channel，使下次 execute 重建连接而非在 broken channel 上重试
    if let Err(e) = setup_result {
        *self.channel_writer.lock().await = None;
        *self.channel_reader.lock().await = None;
        return Err(e);
    }

    // 2. 发起 turn（只需 writer）
    // R21: 用 if let Some 替代 unwrap()，防止 close() 在 step 1e→2 间隙执行致 panic
    let thread_id = self.thread_id.lock().await.clone()
        .ok_or_else(|| AppError::Executor("connection closed during setup".to_string()))?;
    {
        let writer = self.channel_writer.lock().await;
        let w = writer.as_ref()
            .ok_or_else(|| AppError::Executor("connection closed during setup".to_string()))?;
        self.adapter.start_turn(&thread_id, message, attachments, w).await?;
    }

    // 3. 循环读取远端输出（只需 reader，不阻塞 interrupt 获取 writer）
    self.read_loop(sender).await
}
```

**one-shot 模式（multi_turn=false，解决 R3）**：

> 每次 `execute()` 启动新的 SSH exec，消息嵌入命令本身。进程执行完即退出，不复用 channel。无需 `initialize` / `create_thread` / `start_turn`。

```rust
async fn execute_one_shot(&self, _session_id: &str, message: &str, attachments: &[AttachmentData], sender: &Sender<MessageSegment>) -> AppResult<()> {
    let conn_id = self.agent.connection_id.as_ref()?;

    // 1. 检查远端是否安装
    if !self.strategy.check_installed(&self.ssh_manager, conn_id).await? {
        self.strategy.install(&self.ssh_manager, conn_id).await?;
    }

    // 2. 构造 one-shot 命令（消息嵌入命令）
    let command = self.adapter.build_one_shot_command(&self.agent, message, attachments);
    let env_vars = self.adapter.build_env_vars(&self.agent);

    // 3. SSH exec（仍用 bidirectional，因为审批响应需写入 stdin）
    let SshChannelPair { writer, mut reader } = self.ssh_manager.exec_bidirectional(conn_id, &command, &env_vars).await?;

    // 4. 读取循环（同长连接模式的 read_loop，但用局部 writer/reader）
    loop {
        let line = reader.read_line().await?;
        // R35: parse_line 错误可恢复 — 日志记录 + continue
        let msg = match self.adapter.parse_line(&line) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!("parse_line error (ignoring): {} | line: {}", e, line);
                continue;
            }
        };
        match msg {
            RemoteMessage::Response { .. } => {} // one-shot 通常无 Response
            RemoteMessage::Segment(seg) => sender.send(seg).await?,
            RemoteMessage::ApprovalRequest { id, kind, content, available_decisions } => {
                let (tx, rx) = oneshot::channel();
                self.approval_channels.register(&id, tx).await;
                sender.send(MessageSegment::ApprovalRequest {
                    id: id.clone(), kind, content, available_decisions,
                }).await?;
                let decision = rx.await?;
                let response = self.adapter.format_approval_response(&id, &decision);
                writer.write_line(&response).await?;
            }
            RemoteMessage::TurnCompleted { usage } => {
                if let Some(u) = usage { sender.send(MessageSegment::Usage { info: u }).await?; }
                sender.send(MessageSegment::Done).await?;
                break;
            }
            RemoteMessage::Error { message } => {
                sender.send(MessageSegment::Error { message, retryable: None }).await?;
                break;
            }
            RemoteMessage::ThreadStarted { .. } => {}
            RemoteMessage::Ignored => {}
        }
    }
    // 5. 关闭 channel（不复用）
    // R44: 忽略 close 错误（one-shot 不复用 channel，close 失败无影响）
    reader.abort();
    let _ = writer.close().await;
    Ok(())
}
```

**长连接模式的读取循环（read_loop）**：

```rust
async fn read_loop(&self, sender: &Sender<MessageSegment>) -> AppResult<()> {
    loop {
        let line = {
            let mut reader = self.channel_reader.lock().await;
            // R42: close() 并发可将 channel_reader 置 None，用 ok_or_else 替代 unwrap 防 panic
            let r = reader.as_mut()
                .ok_or_else(|| AppError::Executor("connection closed during read_loop".to_string()))?;
            r.read_line().await?
        }; // reader 锁释放，interrupt 可获取 writer
        // R35: parse_line 错误可恢复 — 日志记录 + continue，不因单行解析错误终止连接
        let msg = match self.adapter.parse_line(&line) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!("parse_line error (ignoring): {} | line: {}", e, line);
                continue;
            }
        };
        match msg {
            RemoteMessage::Response { .. } => {
                // 响应已在 adapter 内部消费（initialize/create_thread）
            }
            RemoteMessage::Segment(seg) => sender.send(seg).await?,
            RemoteMessage::ApprovalRequest { id, kind, content, available_decisions } => {
                // N4: 从 approval_channels 获取 oneshot::Receiver
                let (tx, rx) = oneshot::channel();
                self.approval_channels.register(&id, tx).await;

                sender.send(MessageSegment::ApprovalRequest {
                    id: id.clone(), kind, content, available_decisions,
                }).await?;

                // 等待前端决策（submit_approval → approval_channels → oneshot）
                let decision = rx.await?;
                let response = self.adapter.format_approval_response(&id, &decision);

                // 写入审批响应（只需 writer，不冲突 reader）
                let writer = self.channel_writer.lock().await;
                // R43: close() 并发可将 channel_writer 置 None，用 ok_or_else 替代 unwrap 防 panic
                let w = writer.as_ref()
                    .ok_or_else(|| AppError::Executor("connection closed during approval".to_string()))?;
                w.write_line(&response).await?;
            }
            RemoteMessage::TurnCompleted { usage } => {
                if let Some(u) = usage { sender.send(MessageSegment::Usage { info: u }).await?; }
                sender.send(MessageSegment::Done).await?;
                break;
            }
            RemoteMessage::Error { message } => {
                sender.send(MessageSegment::Error { message, retryable: None }).await?;
                break;
            }
            RemoteMessage::ThreadStarted { .. } => {
                // thread/started 通知，thread_id 已在 create_thread 中获取
            }
            RemoteMessage::Ignored => {}
        }
    }
    Ok(())
}
```

**interrupt 实现（并发安全）**：

```rust
async fn interrupt(&self) -> AppResult<()> {
    let thread_id = self.thread_id.lock().await.clone();
    if let Some(tid) = thread_id {
        if let Some(msg) = self.adapter.format_interrupt(&tid) {
            // 获取 writer 锁 — 不与 reader 锁冲突
            let writer = self.channel_writer.lock().await;
            if let Some(w) = writer.as_ref() {
                w.write_line(&msg).await?;
            }
        }
    }
    Ok(())
}
```

### 7.3 AgentExecutor trait 方法实现

| 方法 | 远程模式实现 | 说明 |
|------|-------------|------|
| `execute` | ✅ 实现 | 双模式：`multi_turn=true` 长连接复用；`multi_turn=false` one-shot 每次 new SSH exec（见 §7.2） |
| `interrupt` | ✅ 实现 | `adapter.format_interrupt(thread_id)` → 通过 `channel_writer` 写入 SSH stdin；返回 `None` 时 `Ok(())` 静默忽略；不与 `channel_reader` 锁冲突；one-shot 模式不支持中断（thread_id 为 None，interrupt 为 no-op，R31） |
| `submit_approval` | ✅ 实现 | 通过 `approval_channels.take()` 取出 oneshot::Sender，发送 ApprovalDecision（R39） |
| `close` | ✅ 实现 | 关闭 SSH channel，清空 `channel_writer` / `channel_reader` / `thread_id` / `capabilities` |
| `compact` | ✅ 条件实现 | 需先获取 `turn_lock` 互斥（防止与 read_loop 并发冲突 reader 锁，R38）；检查 `capabilities.compact`；若 true 调 `adapter.format_compact(thread_id)` 发送 compact 请求；返回 None 时返回 `Err("compact not supported")` |
| `resume_session` | ❌ 保留默认 | 远端会话在远端管理，不支持本地 rollout 恢复 |
| `fork_session` | ❌ 保留默认 | 远程模式不支持 fork |
| `truncate_thread` | ❌ 保留默认 | 远程模式不支持截断 |
| `get_rollout_path` | ❌ 返回 `None` | 远端会话在远端管理 |
| `set_history` | ❌ 保留默认 | 远程模式不支持预设历史 |

**submit_approval 实现（R39）**：

```rust
async fn submit_approval(&self, approval_id: &str, decision: ApprovalDecision) -> AppResult<()> {
    match self.approval_channels.take(approval_id).await {
        Some(tx) => {
            tx.send(decision).map_err(|_| {
                AppError::Executor("approval receiver dropped (execute loop exited)".to_string())
            })
        }
        None => Err(AppError::Executor(format!(
            "approval_id '{}' not found (already completed or cancelled)", approval_id
        ))),
    }
}
```

### 7.4 SSH 双向通道

```rust
/// 替代 exec_stream，提供持续读写能力
/// 返回 SshChannelPair（writer + reader 独立，无共享 Mutex）
pub async fn exec_bidirectional(
    &self,
    connection_id: &str,
    command: &str,
    env_vars: &HashMap<String, String>,
) -> AppResult<SshChannelPair>;
```

**实现逻辑**：
1. 通过 SSH `exec` 启动远端命令，获取 `russh::Channel<Msg>`
2. 将 Channel 包为 `Arc<Channel<Msg>>`
3. 创建 `mpsc::channel(256)`（行缓冲）
4. `tokio::spawn(reader_task(channel.clone(), line_tx))` — reader task 持续读取 stdout 行
5. 返回 `SshChannelPair { writer: SshChannelWriter { channel }, reader: SshChannelReader { line_rx, reader_task: Some(task) } }`

**与 `exec_stream` 的关系**：`exec_bidirectional` **替代** `exec_stream`。`exec_stream` 废弃，在实现完成后删除。

**SSH keepalive（解决 N9）**：

> 长连接可能因网络中断、防火墙超时、SSH 服务端 `ClientAliveInterval` 配置等原因断开。需配置 keepalive 防止连接 stale。

| 配置项 | 值 | 说明 |
|--------|---|------|
| `keepalive_interval` | 30 秒 | russh session 每 30 秒发送 SSH keepalive 包 |
| `keepalive_max_failures` | 3 | 连续 3 次 keepalive 无响应 → 判定连接断开 |
| 检测方式 | `read_line` 返回 `Err(SshChannelClosed)` | reader task 检测到 EOF 或读取错误 |
| 恢复策略 | 自动重建 | 下次 `execute()` 发现 `channel_writer.is_none()` → 重新走首次连接流程 |

**close() 远端进程清理（解决 N10）**：

> `close()` 需确保远端进程退出，避免僵尸进程。

```rust
async fn close(&self) -> AppResult<()> {
    // 1. 先关闭 writer（发送 EOF → reader task 收到 EOF → mpsc 推送 Err
    //    → read_line() 返回 Err → read_loop 退出释放 channel_reader 锁）
    //    必须先关 writer 再获取 reader，否则与 read_loop 的 channel_reader 锁死锁
    //    R26: 忽略 close 错误，确保后续清理始终执行
    if let Some(writer) = self.channel_writer.lock().await.take() {
        let _ = writer.close().await;
    }
    // 2. 再获取 reader 锁（read_loop 已因 EOF 退出，锁可获取）
    if let Some(mut reader) = self.channel_reader.lock().await.take() {
        reader.abort();
    }
    // 3. 取消所有待审批的 oneshot（使 read_loop 中 rx.await 返回 Err 而退出）
    self.approval_channels.cancel_all().await;
    // 4. 清空状态
    *self.thread_id.lock().await = None;
    *self.capabilities.lock().await = None;
    Ok(())
}
```

> **关键（解决 R19）**：`close()` 必须先关 `channel_writer` 再获取 `channel_reader`。若先锁 `channel_reader`，而 `read_loop` 正持有该锁等待 `read_line()`，则死锁。先关 writer 使 reader task 收到 EOF，mpsc 推送 Err，`read_line()` 返回 Err，`read_loop` 退出释放锁。`approval_channels.cancel_all()` 取消待审批的 oneshot，使 `read_loop` 中 `rx.await` 返回 Err 而退出。

| 步骤 | 操作 | 超时 |
|------|------|------|
| 1 | `SshChannelWriter::close()` — close channel（发送 EOF） | 立即 |
| 2 | reader task 收到 EOF → mpsc 推送 Err → `read_line()` 返回 Err → `read_loop` 退出 | 毫秒级 |
| 3 | `SshChannelReader::abort()` — 停止 reader task（已退出，清理 JoinHandle） | 立即 |
| 4 | `approval_channels.cancel_all()` — 取消待审批 oneshot | 立即 |
| 5 | 远端收到 stdin EOF → 进程退出 | 5 秒超时 |
| 6 | 若远端未退出，SSH channel close 强制终止 | 由 SSH session 管理 |
| 7 | 清空 `thread_id` / `capabilities` | 立即 |

---

## 8. 数据流

### 8.1 首次消息（建立长连接）

```
前端 → Tauri send_message
  → SshAgentExecutor.execute()
    → [首次] strategy.check_installed() / install()    (第1层)
    → [首次] adapter.build_start_command()              (第2层)
    → [首次] SSH exec command → 建立 SshChannelPair (writer + reader)
    → [首次] adapter.initialize()                       (握手, 获取 capabilities)
    → [首次] adapter.create_thread()                    (创建 thread, 获取 thread_id)
    → adapter.start_turn(thread_id, message, attachments)  (发起 turn)
    → loop:
      → adapter.parse_line()                            (解析)
      → Segment → 前端
      → ApprovalRequest → 前端弹窗 → 用户决策 → adapter.format_approval_response() → SSH stdin
      → TurnCompleted → Done → break (不关闭 channel)
```

### 8.2 后续消息（复用长连接 / one-shot）

**multi_turn=true（长连接复用）**：
```
前端 → Tauri send_message
  → SshAgentExecutor.execute()
    → [channel 已存在] 跳过安装/启动/握手/create_thread
    → adapter.start_turn(thread_id, message, attachments)  (复用 thread_id)
    → loop: 同首次
```

**multi_turn=false（one-shot，每次新 SSH exec）**：
```
前端 → Tauri send_message
  → SshAgentExecutor.execute()
    → adapter.build_one_shot_command(agent, message, attachments)  (消息嵌入命令)
    → SSH exec → 新 SshChannelPair
    → loop: parse_line → Segment/Approval/TurnCompleted
    → close channel（不复用）
```

### 8.3 用户中断

```
前端 → Tauri interrupt
  → adapter.format_interrupt(thread_id)
  → 若 Some(msg): 写入 SSH stdin
  → 若 None: Ok(()) 静默忽略
```

### 8.4 用户审批

```
前端 → Tauri submit_approval
  → approval_channels oneshot 传递 ApprovalDecision
  → execute 循环中收到决策 → adapter.format_approval_response(request_id, decision)
  → 写入 SSH stdin
```

### 8.5 关闭连接

```
前端 → Tauri close / 应用退出
  → SshAgentExecutor.close()
  → SshChannelWriter::close()（发送 SSH channel close / EOF）
  → reader task 收到 EOF → mpsc 推送 Err → read_loop 退出释放 channel_reader 锁
  → SshChannelReader::abort()（清理 reader task）
  → approval_channels.cancel_all()（取消待审批 oneshot）
  → 远端进程收到 stdin EOF + channel close → 退出
  → 清空 channel_writer / channel_reader / thread_id / capabilities
  → 超时：若远端 5 秒内未退出，SSH session 强制终止
```

---

## 9. 改动范围

### 9.1 新增文件

| 文件 | 职责 |
|------|------|
| `executor/remote_protocol/mod.rs` | `RemoteProtocolAdapter` trait + `RemoteMessage` + `RemoteCapabilities` + `SshChannelWriter` + `SshChannelReader` + `SshChannelPair` + 工厂函数 |
| `executor/remote_protocol/codex_adapter.rs` | `CodexAppServerAdapter` |
| `executor/remote_protocol/openclaw_adapter.rs` | `OpenClawAdapter` |
| `executor/remote_protocol/claude_code_adapter.rs` | `ClaudeCodeAdapter` |
| `executor/remote_protocol/json_rpc.rs` | JSON-RPC 消息编解码（`JSONRPCMessage`、`JSONRPCRequest` 等） |

### 9.2 修改文件

| 文件 | 改动 | 工作量 |
|------|------|--------|
| `executor/ssh/agent.rs` | 重写：双模式（长连接 + one-shot）+ 双层策略 + 多 turn | 大 |
| `ssh/manager.rs` | 新增 `exec_bidirectional`；废弃 `exec_stream` | 中 |
| `domain/strategy/factory.rs` | 创建 adapter + strategy 并传入 `SshAgentExecutor::new` | 小 |

### 9.3 废弃文件

| 文件 | 处理 |
|------|------|
| `executor/jsonl.rs` | 废弃，由 `remote_protocol/` 替代 |
| `ssh/manager.rs::exec_stream` | 废弃，由 `exec_bidirectional` 替代 |

### 9.4 不变文件

| 文件 | 说明 |
|------|------|
| `executor/local/*` | 本地库嵌入模式完全不变 |
| `domain/strategy/remote_agent.rs` | `RemoteAgentStrategy` trait 不变（已有） |
| `domain/model/mod.rs` | `Agent` + `RemoteAgentType` 不变（已有） |
| `domain/strategy/mod.rs` | `AgentExecutor` trait + `MessageSegment` + `ApprovalDecision` 不变 |
| 前端所有文件 | `MessageSegment` 协议不变 |

---

## 10. 风险与缓解

| 风险 | 等级 | 缓解措施 |
|------|:---:|---------|
| codex app-server 协议变更 | 中 | 锁定 codex-rs 版本；initialize 中协商版本 |
| 远端版本不匹配 | 中 | initialize 握手时检测协议版本；不兼容时返回友好错误提示升级 |
| openclaw 协议变更 | 低 | v2026.9.8 已实测确认；JSON envelope 格式稳定；锁定版本 |
| claude code MCP 模式不可用 | 中 | 优先 MCP，启动失败降级 JSONL；`capabilities()` 反映实际模式 |
| 远端未安装 | 中 | 第1层策略自动检测+安装 |
| SSH 连接断开 | 中 | `read_line` 返回 Err → 发 `Error` segment → 清空 channel → 下次 execute 重建 |
| SSH keepalive 失败 | 低 | 30 秒 keepalive 间隔；连续 3 次失败判定断开；自动重建连接（N9） |
| 审批超时 | 低 | 前端审批弹窗加超时计时器（默认 5 分钟） |
| 并发 turn | 中 | `turn_lock` 互斥锁，同一 executor 不允许并发 turn，返回 `Err("turn in progress")` |
| 长连接内存泄漏 | 低 | `close()` 确保清空所有状态；SSH 断开时 `read_line` 返回 Err 触发清理 |
| 能力差异处理 | 低 | `RemoteCapabilities` 声明能力，降级处理（如 `compact` 不支持时返回 Err） |
| 远端僵尸进程 | 低 | `close()` 先关 writer（EOF）再获取 reader 锁，避免与 read_loop 死锁（R19）；5 秒超时后 SSH session 强制终止（N10） |
| close() 与 read_loop 死锁 | 中 | `close()` 先关 `channel_writer` 发 EOF → read_loop 退出释放 `channel_reader` 锁 → 再获取 reader 清理；`approval_channels.cancel_all()` 取消待审批（R19） |
| reader task panic | 低 | reader task 内部 catch_unwind；panic 时推送 Err 到 mpsc → execute 返回错误 → 重建连接 |

---

## 11. 验收标准

| # | 标准 | 验证方式 |
|---|------|---------|
| V1 | codex 远程模式可正常对话 | 发送消息 → 收到回复 |
| V2 | codex 审批工作 | 命令审批 → 批准/拒绝 → 结果 |
| V3 | codex 多 turn 对话 | 连续发 3 条消息 → 复用连接 → 上下文连续 |
| V4 | claude code 远程模式可用 | 发送消息 → 收到回复 |
| V5 | openclaw 远程模式可用 | 发送消息 → 收到 JSON envelope → 解析回复（one-shot 模式） |
| V6 | 配置传递正确 | 指定 model/base_url → 远端使用指定配置 |
| V7 | 中断工作 | 发送中断 → AI 停止 |
| V8 | Token 使用量显示 | TurnCompleted 含 usage → 前端显示 |
| V9 | 流式输出 | item/updated → 前端实时更新 |
| V10 | 本地模式不受影响 | 本地模式所有功能正常 |
| V11 | 跨平台 | 远程 Linux + 本地 Windows 正常 |
| V12 | 策略切换 | 修改 Agent 的 remote_agent_type 后重新连接，使用对应远端 |
| V13 | 连接断开恢复 | SSH 断开后重发消息 → 自动重建连接 |
| V14 | 并发 turn 拒绝 | turn 进行中再发消息 → 返回错误提示 |

---

## 12. 实施计划

> **实施顺序（2026-10-07 确定）**：先实施 openclaw（one-shot JSON envelope），后实施 codex（长连接 JSON-RPC）。openclaw 协议已远程实测确认（v2026.9.8），one-shot 模式简单（无握手/长连接），先验证核心架构（`RemoteProtocolAdapter` trait + `exec_bidirectional` + `SshAgentExecutor` one-shot 路径），再实施复杂的 codex 长连接模式。

| 阶段 | 任务 | 依赖 | 优先级 |
|------|------|------|--------|
| S1 | `RemoteProtocolAdapter` trait + `RemoteCapabilities` + `RemoteMessage` + `SshChannelWriter/Reader/Pair` | 无 | P0 |
| S2 | `SshManager::exec_bidirectional` + 单元测试 | 无 | P0 |
| S3 | `OpenClawAdapter` 实现（one-shot JSON envelope，协议已确认） | S1 | P0 |
| S4 | `SshAgentExecutor` one-shot 模式重写 | S2, S3 | P0 |
| S5 | 集成测试（openclaw 远程对话 + JSON envelope 解析） | S4 | P0 |
| S6 | JSON-RPC 消息编解码 (`json_rpc.rs`) | 无 | P1 |
| S7 | `CodexAppServerAdapter` 实现（长连接） | S1, S6 | P1 |
| S8 | `SshAgentExecutor` 长连接模式扩展 | S7 | P1 |
| S9 | 集成测试（codex 长连接 + 审批 + 多 turn） | S8 | P1 |
| S10 | `ClaudeCodeAdapter` 实现 ✅ | S1, S6 | P2 |
| S11 | 集成测试（claude code） ✅ | S10 | P2 |
| S12 | 远程 codex-rs 编译部署（新建独立工程 `botwork-cloud-server`（`D:\trae-work-space\mobi-work\botwork-cloud-server\`）含 `adapter-shared` + `server` crate，远程原生编译 aarch64 Linux，修改 `build_start_command` 用 `agent.command` + `--api-key` 提取） | S8 | P1 |
| S13 | 远程 codex-rs 端到端测试（编译版 + 适配器 + Agnes API 完整链路） | S12 | P1 |

关键路径：S1 → S3 → S4 → S5（openclaw one-shot 先验证）→ S6 → S7 → S8 → S12 → S13（codex 长连接 + 远程编译部署）

**P0（openclaw one-shot）优先实现**，P1（codex 长连接 + 远程编译部署）后续迭代，P2（claude code）最后。

---

## 14. 远程 codex-rs 编译部署方案（v2.10.0 新增）

### 14.1 问题背景

远程 SSH 模式的 codex 端到端测试（S8/v2.9.0）发现：远程通过 npm 安装的**官方原版 codex CLI v0.160.1** 不含本地 codex-rs 版本中附加的 **Responses API ↔ Chat Completions 适配器**（`src/adapter/` 中的 axum HTTP 代理），导致：

- 官方 codex 仅支持 Responses API（`wire_api = "responses"`），`wire_api = "chat"` 已移除
- Agnes API（`apihub.agnes-ai.com`）仅支持 Chat Completions API（`/v1/chat/completions`）
- 两者端点不兼容 → 模型调用返回 `unknown variant 'namespace'` 等 400 错误
- 长连接管道本身（SSH → app-server → initialize → thread/start → turn/start → 通知接收）完全正常，问题出在模型调用层

**本地 codex-rs**（bot-work-windows 集成）已内置适配器解决此问题：`codex_config.rs` 中 `detect_api_type()` 检测 API 类型，若为 Chat Completions API 则启动 `ApiAdapter`（axum 本地代理），将 codex-rs 的 `openai_base_url` 指向适配器，适配器做 Responses ↔ Chat Completions 格式转换。

### 14.2 设计决策

**将本地 codex-rs + 适配器编译为 aarch64 Linux 二进制部署到远程机器**，bot-work-windows SSH 远程模式连接该编译版而非官方 codex CLI，保证功能与本地一致。

### 14.3 方案选择：方案 A — 单二进制

新建独立 binary crate（`botwork-cloud-server`），集成 adapter + converter + codex-rs app-server 代码，编译为单个 aarch64 Linux 二进制。

**架构**：
```
bot-work-windows (Windows)                远程机器 (aarch64 Linux)
┌──────────────────────┐                 ┌─────────────────────────────────┐
│  SshAgentExecutor    │                 │  botwork-cloud-server (单二进制)  │
│  ├── SSH 连接 ──────────────────────→ │  ├── 内置 ApiAdapter (axum 代理) │
│  │   JSON-RPC over stdio              │  │   ├── detect_api_type()       │
│  │   ← 通知/响应                       │  │   └── Responses ↔ Chat 转换   │
│  └── CodexAppServerAdapter            │  └── codex app-server (codex-rs) │
└──────────────────────┘                 │      └── base_url → 内置适配器   │
                                         └─────────────────────────────────┘
                                                    ↓
                                         Agnes API (apihub.agnes-ai.com)
```

**启动流程**：
1. bot-work-windows SSH 到远程，执行 `botwork-cloud-server --listen stdio:// -c model=... -c model_providers.openai.base_url=... --api-key=...`
2. `botwork-cloud-server` 启动时：
   a. 解析 `-c` 参数获取 model/base_url，`--api-key` 获取 API key（内部设为环境变量供 codex app-server 的 `env_key` 读取）
   b. 调用 `detect_api_type()` 检测上游 API 类型
   c. 若为 Chat Completions API → 启动内置 `ApiAdapter`（绑定 `127.0.0.1:0` 随机端口）
   d. 将 codex app-server 的 `openai_base_url` 指向内置适配器
   e. 启动 codex app-server（调用 `codex_app_server::run_main_with_transport_options()`，JSON-RPC over stdio）
3. 后续 JSON-RPC 通信流程不变（initialize → thread/start → turn/start → 通知）

> **stdio 占用说明（R9）**：`run_main_with_transport_options()` 接管 stdin/stdout 进行 JSON-RPC 通信，适配器 `ApiAdapter` 作为后台 tokio task 运行（axum 绑定 `127.0.0.1:随机端口`，不占用 stdio）。`botwork-cloud-server` 的 main.rs 在调用 `run_main_with_transport_options()` 前先启动适配器并修改 `cli_config_overrides` 中的 `openai_base_url`，然后移交控制权给 app-server。无 stdio 争抢。

### 14.4 实现设计

#### 14.4.1 新建 crate：`botwork-cloud-server`

**位置**：`D:\trae-work-space\mobi-work\botwork-cloud-server\`（与 `bot-work-windows/` 同级，独立工程）

**Cargo.toml**：
```toml
[package]
name = "botwork-cloud-server"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "botwork-cloud-server"
path = "src/main.rs"

[dependencies]
# codex-rs 子 crate（与 src-tauri 相同的 path 依赖）
codex-core = { path = "../vendor/codex/codex-rs/core" }
codex-protocol = { path = "../vendor/codex/codex-rs/protocol" }
codex-exec-server = { path = "../vendor/codex/codex-rs/exec-server" }
codex-config = { path = "../vendor/codex/codex-rs/config" }
# ... 其他 codex-rs 子 crate

# 适配器依赖
axum = { version = "0.8", features = ["http1", "tokio"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls-native-roots"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
# 共享适配器 crate
adapter-shared = { path = "../adapter-shared" }
```

> **完整依赖列表（R10）**：上述仅展示关键依赖，codex-rs app-server 完整依赖 60+ 子 crate，详见 `vendor/codex/codex-rs/app-server/Cargo.toml`。`botwork-cloud-server` 的 Cargo.toml 应直接复制 app-server 的 `[dependencies]` 段，再追加 `adapter-shared`。

**不含的依赖**（关键排除项）：
- `tauri` 及所有 tauri-plugin-* — 无 GUI
- `windows` crate — 非 Windows 平台
- `russh` / `russh-keys` — 不需要 SSH 客户端
- `rusqlite` — 不需要本地数据库

> **依赖范围说明（R2）**：codex-rs app-server 依赖 60+ codex-* 子 crate（codex-core, codex-protocol, codex-exec-server, codex-config, codex-login, codex-history, codex-thread-store 等），编译时间长（远程 7 核预计 10-30 分钟）。但所有 crate 已 vendored 在 `vendor/codex/codex-rs/`，且适配器代码（`adapter/mod.rs` + `converter.rs`）和 `detect_api_type()` 均自包含（仅依赖 axum/reqwest/serde_json/tokio/futures），无 bot-work-windows 耦合，可直接抽取。

#### 14.4.2 代码结构

```
botwork-cloud-server/
├── Cargo.toml          # 依赖 adapter-shared + codex-rs app-server 全部子 crate
└── src/
    └── main.rs          # 入口：解析参数 → detect_api_type → 启动适配器 → 修改 base_url → 调用 run_main_with_transport_options()
```

> **代码来自 adapter-shared（R11 修复）**：适配器/转换器/API 检测代码不在 `botwork-cloud-server/src/` 中，而是通过 `adapter-shared` crate 依赖引入。`main.rs` 调用 `adapter_shared::api_detect::detect_api_type()` 和 `adapter_shared::adapter::ApiAdapter::start()`。

> **参数兼容性设计（R6）**：`botwork-cloud-server` 接受与 codex app-server 相同的 `-c key=value` 参数（通过 `clap` flatten `CliConfigOverrides`），额外增加 `--api-key <KEY>` 参数（用于适配器 API 检测和代理转发）。`--listen` 参数直接传递给 app-server。

#### 14.4.3 编译方式：远程原生编译（R1 修复）

> **原方案为从 Windows 交叉编译到 `aarch64-unknown-linux-gnu`，但 Windows 开发机无 WSL/Docker/cross/aarch64-gcc，交叉编译不可行。改为远程原生编译。**

**远程机器环境**（已确认）：
- Android 内核 5.15.180 + **Ubuntu userspace（GLIBC 2.39）**（非 Android bionic libc）
- GCC 13.3.0（Ubuntu 13.3.0-6ubuntu2~24.04.1）
- 7 CPU 核心，22GB RAM，557GB 可用磁盘
- `/lib/ld-linux-aarch64.so.1` 存在（GNU 动态链接器）

**编译步骤**：
1. 远程安装 Rust 工具链：`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
2. rsync 增量同步源码到远程（R12 修复）：`rsync -avz --exclude target botwork-cloud-server/ root@remote:/root/botwork-cloud-server/` + `rsync -avz --exclude target bot-work-windows/vendor/codex/codex-rs/ root@remote:/root/bot-work-windows/vendor/codex/codex-rs/`
3. 远程原生编译：`cargo build --release`（无需交叉编译工具链）
4. 产物：`botwork-cloud-server/target/release/botwork-cloud-server`（aarch64 GNU ELF）

> **远程目录布局（R13 修复）**：远程编译时须保持与本地相同的相对目录结构，因 Cargo.toml 用 `path = "../../bot-work-windows/vendor/codex/codex-rs/core"` 引用依赖：
> ```
> /root/
> ├── botwork-cloud-server/      # botwork-cloud-server 工程源码（workspace）
> │   ├── adapter-shared/      # 共享适配器 crate
> │   ├── server/              # binary crate
> │   └── Cargo.toml           # workspace 根
> └── bot-work-windows/        # bot-work-windows 工程（仅需 vendor/）
>     └── vendor/codex/codex-rs/   # codex-rs 全部子 crate 源码
> ```

**编译资源需求**：
- 60+ codex-* 子 crate，预计编译时间 10-30 分钟（7 核并行）
- 峰值内存约 2-4GB（Rust 编译内存占用）
- 磁盘约 5-10GB（target 目录）

#### 14.4.4 部署方式

1. 远程原生编译 `botwork-cloud-server` → 产物在 `target/release/botwork-cloud-server`
2. 部署到 `/usr/local/bin/botwork-cloud-server`（或编译目录直接引用）
3. bot-work-windows 创建 codex agent 时 `command` 字段设为 `botwork-cloud-server`（而非 `codex`）
4. `CodexAppServerAdapter::build_start_command` 构造命令：使用 `agent.command` 字段（如 `botwork-cloud-server`）替代硬编码 `codex app-server`，追加 `--listen stdio:// -c model=... -c model_providers.openai.base_url=... --api-key=...`（`--api-key` 从 `agent.tool_env_vars` 提取）

#### 14.4.5 与现有代码的关系

**代码共享策略（R3 修复）**：将适配器代码抽取为共享 crate `adapter-shared`，避免复制/粘贴维护：

```
mobi-work/
├── botwork-cloud-server/      # 独立工程（云端 codex-rs + 适配器）
│   ├── Cargo.toml           # workspace 根
│   ├── adapter-shared/      # 共享适配器 crate
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs       # pub mod adapter; pub mod converter; pub mod api_detect;
│   │       ├── adapter.rs   # 从 bot-work-windows/src-tauri/src/adapter/mod.rs 迁移
│   │       ├── converter.rs # 从 bot-work-windows/src-tauri/src/adapter/converter.rs 迁移
│   │       └── api_detect.rs # detect_api_type 抽取
│   └── server/              # botwork-cloud-server binary crate
│       ├── Cargo.toml       # 依赖 adapter-shared + codex-rs app-server
│       └── src/main.rs      # 入口
└── bot-work-windows/        # 主工程
    └── src-tauri/           # 依赖 adapter-shared（本地集成模式 re-export）
```

- `adapter-shared` 仅依赖 axum/reqwest/serde_json/tokio/futures（已确认无 bot-work-windows 耦合）
- `src-tauri/src/adapter/` 改为 re-export `adapter-shared`（保持现有引用不变）
- 两处代码自动同步（同一 crate 源码）

> **本地集成模式回归风险（R15）**：`src-tauri/src/adapter/` 重构为 re-export `adapter-shared` 后，须验证本地集成模式（codex-rs 库嵌入 + 适配器）功能不受影响——适配器启动、API 类型检测、Responses ↔ Chat Completions 转换均须回归测试通过。

**build_start_command 修改（R4/R7 修复）**：不自动检测远程是否部署 `botwork-cloud-server`，改为通过 `Agent.command` 字段配置：
- 用户创建 agent 时 `command` 设为 `botwork-cloud-server`（远程编译版）或 `codex`（官方版）
- `build_start_command` 使用 `agent.command` 拼接 ` app-server --listen stdio://`（而非硬编码 `codex app-server`）
- `--api-key` 从 `agent.tool_env_vars` 中提取（JSON 解析获取 `AGNES_API_KEY` 值）

### 14.5 验收标准

1. `botwork-cloud-server` 可在 aarch64 Linux 上独立运行
2. 启动时自动检测 API 类型并启动适配器
3. bot-work-windows SSH 连接 `botwork-cloud-server`，完整对话流程通过（用户发消息 → assistant 回复）
4. Agnes API（Chat Completions only）通过适配器成功响应
5. `adapter-shared` 重构后本地集成模式回归测试通过（R16 补充）

---

## 15. anthropic_adapter.py 合并方案（v2.15.0 新增）

### 15.1 背景

当前 claude code 远程模式依赖独立的 Python 适配器 `anthropic_adapter.py`（固定端口 8082）做 Anthropic Messages API ↔ OpenAI Chat Completions 协议转换。这是因为 botwork-cloud-server 的内置适配器绑定**随机端口**（`127.0.0.1:0`），而 claude code CLI 需要通过 `ANTHROPIC_BASE_URL` 环境变量指向**固定可预测**的端点。

**问题**：Python 适配器是临时绕行方案，导致：
1. 远程主机需要额外安装 Python 3 + 依赖
2. 两个适配器并存（Rust adapter 服务 codex，Python adapter 服务 claude code），维护负担加倍
3. 协议转换逻辑重复实现（Rust `anthropic_converter.rs` + Python `anthropic_adapter.py`）
4. 部署脚本需要额外管理 Python 适配器的启动/停止/健康检查

### 15.2 现状分析

**Rust 端已实现**（v2.12.0）：

| 文件 | 已有内容 | 行号 |
|------|---------|------|
| `anthropic_converter.rs` | `anthropic_to_chat_completions()` 请求转换 | :7 |
| `anthropic_converter.rs` | `chat_completions_to_anthropic()` 响应转换 | :68 |
| `anthropic_converter.rs` | `convert_anthropic_sse_stream()` 流式 SSE 转换 | :123 |
| `anthropic_converter.rs` | 14 单元测试 | :319+ |
| `adapter.rs` | `/v1/messages` 路由注册 | :46 |
| `adapter.rs` | `handle_messages` handler | :193 |
| `adapter.rs` | `proxy_anthropic_chat_response` 响应代理 | :225 |

**Python 适配器有但 Rust 端缺的功能**：

| # | 功能 | Python 实现 | Rust 端状态 |
|---|------|------------|------------|
| F1 | **固定端口** | `PORT = int(os.environ.get("ADAPTER_PORT", "8082"))` | ❌ `127.0.0.1:0` 随机 |
| F2 | **模型名映射** | `MODEL_MAP: claude-* → agnes-*` | ❌ 直接透传 model |
| F3 | **强制 stream=false** | `chat["stream"] = False`（Agnes 不支持流式） | ❌ 透传 stream 参数 |
| F4 | **HEAD 健康检查** | `do_HEAD` 处理 `/hello` `/health` | ❌ 无 HEAD 路由 |
| F5 | **SSE 伪流式** | 上游非流式 → 客户端流式（拆分响应为 SSE 事件） | ❌ 上游非流式 → 客户端非流式 |

### 15.3 合并方案

#### 15.3.1 固定端口（F1）

`ApiAdapter::start` 签名增加 `port: Option<u16>` 参数：

```rust
pub async fn start(
    real_base_url: String,
    api_key: String,
    use_chat_completions: bool,
    port: Option<u16>,           // 新增：None=随机, Some(p)=固定端口
) -> Result<Self, String>
```

绑定逻辑：
```rust
let bind_addr = match port {
    Some(p) => format!("127.0.0.1:{p}"),
    None    => "127.0.0.1:0".to_string(),
};
let listener = tokio::net::TcpListener::bind(&bind_addr).await...
```

`main.rs` 新增 CLI 参数：
```rust
#[arg(long = "adapter-port", value_name = "PORT", env = "ADAPTER_PORT")]
adapter_port: Option<u16>,
```

启动时传入：`ApiAdapter::start(base_url, api_key, true, adapter_port).await`

#### 15.3.2 模型名映射（F2）

在 `anthropic_converter.rs` 中添加模型映射函数：

```rust
fn map_model(model: &str) -> String {
    match model {
        "claude-sonnet-4-20250514" | "claude-3-5-sonnet-20241022" | "claude-3-5-sonnet-latest"
            => std::env::var("AGNES_MODEL").unwrap_or_else(|_| "agnes-3.0-flash".into()),
        "claude-3-opus-20240229" | "claude-3-opus-latest"
            => "agnes-2.5-pro".to_string(),
        "claude-3-haiku-20240307" | "claude-3-haiku-latest"
            => "agnes-2.0-flash".to_string(),
        _ if model.starts_with("claude-") => "agnes-3.0-flash".to_string(),
        _ => model.to_string(),
    }
}
```

在 `anthropic_to_chat_completions` 中调用：
```rust
if let Some(model) = req.get("model").and_then(|v| v.as_str()) {
    chat_req["model"] = json!(map_model(model));
}
```

#### 15.3.3 强制 stream=false + SSE 伪流式（F3 + F5）

**问题**：Agnes API 不支持流式（`stream=true` 会报错），但 claude code CLI 发送 `stream=true` 期望 SSE 响应。

**方案**：在 `handle_messages` 中：
1. 记算客户端期望的 `is_stream`（原始请求的 stream 字段）
2. 转换为 Chat Completions 时**强制 `stream=false`**（发到上游的请求非流式）
3. 上游返回完整 JSON 响应后：
   - 若 `is_stream=true`：将完整响应拆分为 SSE 事件序列（message_start → content_block_delta → message_delta → message_stop），以 `text/event-stream` 返回
   - 若 `is_stream=false`：直接返回完整 JSON

在 `anthropic_converter.rs` 中新增：
```rust
/// 将非流式 Anthropic 响应拆分为 SSE 事件流
pub fn anthropic_response_to_sse_stream(resp: Value) -> impl Stream<Item = Result<String, std::io::Error>>
```

生成事件序列：
1. `message_start`（含 message 元数据）
2. `content_block_start`（index=0, type=text）
3. `content_block_delta`（text_delta，完整文本一次性发出）
4. `content_block_stop`
5. `message_delta`（stop_reason + usage）
6. `message_stop`

#### 15.3.4 HEAD 健康检查（F4）

在 `adapter.rs` 路由中添加：
```rust
.route("/hello", get(|| async { "ok" }))
.route("/health", get(|| async { "ok" }))
```

同时添加 HEAD 方法支持（axum 的 `get` handler 自动支持 HEAD）。

#### 15.3.5 部署脚本变更

**`common.sh`**：
- 删除 `deploy_adapter()` 函数（Python 适配器部署逻辑）
- 新增 `configure_claude_code()` 函数：设置 `ANTHROPIC_BASE_URL=http://127.0.0.1:${ADAPTER_PORT}` 和 `ANTHROPIC_API_KEY`

**`deploy-*.sh`**：
- 主流程中 `deploy_adapter` 替换为 `configure_claude_code`
- `INSTALL_CLOUD_SERVER` 默认改为 `yes`（botwork-cloud-server 现在是必需的，不再是可选）

**`deploy-cloud-server.sh`**：
- 启动 botwork-cloud-server 时添加 `--adapter-port ${ADAPTER_PORT}` 参数
- 确保 `AGNES_API_KEY` 环境变量传递

**删除文件**：
- `deploy/anthropic_adapter.py`（不再需要）

#### 15.3.6 bot-work-windows 端变更

`ClaudeCodeAdapter::build_env_vars()`：
- `ANTHROPIC_BASE_URL` 改为 `http://127.0.0.1:{adapter_port}`（不带 `/v1` 后缀，claude code 自动追加 `/v1/messages`）
- `ANTHROPIC_API_KEY` 保持不变

`ClaudeCodeAdapter::build_one_shot_command()`：
- 无需改动（环境变量通过 `export` 前缀传递）

### 15.4 改动文件清单

| 文件 | 改动 | 优先级 |
|------|------|--------|
| `adapter-shared/src/adapter.rs` | F1 固定端口 + F4 HEAD 路由 + F3 stream=false 逻辑 | P0 |
| `adapter-shared/src/anthropic_converter.rs` | F2 模型映射 + F5 SSE 伪流式 | P0 |
| `server/src/main.rs` | F1 `--adapter-port` CLI 参数 | P0 |
| `deploy/common.sh` | 删除 deploy_adapter + 新增 configure_claude_code | P1 |
| `deploy/deploy-*.sh` (7 个) | deploy_adapter → configure_claude_code | P1 |
| `deploy/deploy-cloud-server.sh` | 添加 --adapter-port 启动参数 | P1 |
| `deploy/anthropic_adapter.py` | **删除** | P1 |
| `deploy/README.md` | 更新部署文档 | P2 |
| `bot-work-windows/.../claude_code_adapter.rs` | ANTHROPIC_BASE_URL 指向固定端口 | P1 |

### 15.5 验收标准

1. `cargo test -p adapter-shared` 全部通过（含新增模型映射 + SSE 伪流式测试）
2. botwork-cloud-server 以 `--adapter-port 8082` 启动，`/v1/messages` 端点可用
3. `curl -X HEAD http://127.0.0.1:8082/hello` 返回 200
4. claude code CLI 以 `ANTHROPIC_BASE_URL=http://127.0.0.1:8082` 发送请求，获得正确响应
5. 远程部署后无需 Python 适配器，claude code 端到端测试通过
6. codex 端到端测试无回归（`/v1/responses` 路由不受影响）

### 15.6 风险与缓解

| # | 风险 | 缓解 |
|---|------|------|
| R1 | 固定端口冲突 | 启动前检测端口占用，冲突时报清晰错误；端口可通过 `--adapter-port` 配置 |
| R2 | Agnes 不支持流式但 claude code 发 stream=true | F3 强制 stream=false + F5 SSE 伪流式拆分 |
| R3 | 模型名映射不完整 | 未映射的 `claude-*` 统一回退到 `agnes-3.0-flash`；非 claude 模型名透传 |
| R4 | codex app-server 退出后 adapter 端口释放 | adapter 生命周期与 app-server 绑定，claude code 在 app-server 运行期间使用 |
| R5 | 本地集成模式回归 | `adapter.rs` 改动不影响 `/v1/responses` 路由；本地模式 port=None 保持随机端口行为 |

---

## 13. 变更记录

| 版本 | 日期 | 变更 |
|------|------|------|
| v1.0.0 | 2026-10-06 | 初始版本 |
| v2.0.0 | 2026-10-06 | 策略模式提升为核心设计；明确支持 codex/openclaw/claude code 三种远端；新增能力协商；新增各远端适配器设计 |
| v2.1.0 | 2026-10-06 | 评审修复：长连接多 turn 设计（R1）；ChannelWriter/Reader 类型定义（R2）；thread 生命周期管理（R3）；trait 方法实现说明（R4）；exec_stream 废弃说明（R5）；capabilities 语义明确（R6）；类型一致性修复（R7）；ClaudeCode 模式选择策略（R8）；factory 签名变更（R9）；interrupt 处理（R10）；风险补充（R11-R12）；验收修正（R13）；依赖修正（R14）；OpenClaw 状态标注（R15）；测试阶段补充（R16）；审批决策映射表（R20） |
| v2.2.0 | 2026-10-06 | 第二轮+第三轮评审修复（11 项）：N1 — SshChannelWriter/Reader 拆分（reader task + mpsc 无锁设计）；N2 — read_line 不持有 Channel 锁（消除死锁风险）；N3 — RemoteMessage 增加 Response 变体（adapter 内部等待请求响应）；N4 — approval_rx 来源说明（oneshot::channel + approval_channels.register）；N5 — thread/start Response vs thread/started 通知澄清（从 Response 提取 thread_id）；N6 — ClaudeCodeAdapter create_thread 语义（MCP 虚拟 thread_id / JSONL --session-id）；N7 — RemoteProtocolAdapter 增加 format_compact 方法；N8 — session_id ↔ thread_id 映射说明（1:1，session_id 仅本地日志关联）；N9 — SSH keepalive 策略（30 秒间隔，3 次失败判定断开）；N10 — close() 远端进程清理（abort reader task + close channel + 5 秒超时）；R1 — Writer/Reader 独立 Mutex（interrupt 与 execute read_line 并发无死锁） |
| v2.3.0 | 2026-10-06 | 第四轮评审修复（5 项）：R2 — trait initialize/create_thread writer 签名从 &mut 改为 &SshChannelWriter（write_line 是 &self 方法）；R3 — 增加 one-shot 模式（multi_turn=false 时每次 execute 新 SSH exec，不复用 channel）+ build_one_shot_command trait 方法 + execute 双模式分支；R4 — execute 代码 ssh 变量改为 self.ssh_manager；R5 — §7.3 close 描述修复（channel → channel_writer/channel_reader）；R6 — §4.4 mod.rs 注释补全 SshChannelWriter/Reader/Pair；R7 — §6.1 channel.read_line() 改为 reader.read_line() |
| v2.4.0 | 2026-10-07 | 第五轮评审修复（6 项）：R8 (P0) — execute_long_connection step 1 双重锁 channel_writer 死锁修复（先检查释放锁再重新获取）；R10 (P1) — execute_one_shot 借用检查器错误修复（解构 SshChannelPair 替代先借用再移动）；R19 (P0) — close() 与 read_loop channel_reader 锁死锁修复（先关 writer 发 EOF 使 read_loop 退出，再获取 reader 清理；新增 approval_channels.cancel_all() 取消待审批）；R16 (P2) — §5.3 能力表 claude code/openclaw multi_turn 改为条件性 ✅/❌（MCP=true, JSONL=false）；R17 (P2) — §12 S2 描述补全 RemoteMessage + SshChannelPair；R18 (P2) — §9.2 agent.rs 描述改为"双模式（长连接 + one-shot）" |
| v2.5.0 | 2026-10-07 | 第六轮评审修复（12 项）：R36 (P0) — execute/execute_long_connection/execute_one_shot 签名增加 attachments: &[AttachmentData] 参数 + start_turn/build_one_shot_command 签名同步更新 + attachments 传递方式说明；R37 (P1) — 新增 ApprovalChannelManager 包装结构体（register/take/cancel_all 方法）替代裸 ApprovalChannels 类型别名 + factory 用 from_channels 包装；R21 (P1) — execute_long_connection step 2 TOCTOU 竞态修复（if let Some + ok_or_else 替代 unwrap()，防止 close() 间隙执行致 panic）；R26 (P1) — close() 中 writer.close() 改为 let _ = 忽略错误确保后续清理始终执行；R29 (P1) — initialize/create_thread 失败时清理 channel（置 channel_writer/reader = None）使下次 execute 重建连接；R35 (P1) — read_loop/execute_one_shot 中 parse_line 错误改为日志记录 + continue（不因单行解析错误终止连接）；R20 (P2) — setup 阶段双锁注明非死锁可接受；R25 (P2) — reader_task 补充 stderr 处理说明（额外 spawn stderr reader task + tracing::warn 日志）；R31 (P2) — one-shot interrupt 描述修正为不支持（thread_id 为 None，no-op）；R38 (P2) — compact 补充 turn_lock 互斥说明；R39 (P2) — submit_approval 补充实现代码（approval_channels.take + send）；R33 (P2) — header 日期修正为 2026-10-07 |
| v2.6.0 | 2026-10-07 | 第七轮评审修复（7 项）：R41 (P1) — setup_result 代码块中 return Err(e) 从函数返回而非从块返回致 R29 清理不可达，改为嵌套 match 以 Err(e) 作为块值返回；R42 (P1) — read_loop 中 reader.as_mut().unwrap() 在 close() 并发时可 panic，改为 ok_or_else 优雅返回 Err；R40 (P2) — SshAgentExecutor 结构体和构造函数 approval_channels 字段缩进统一；R43 (P2) — read_loop 审批响应 writer.as_ref().unwrap() 在 close() 并发极小窗口可 panic，改为 ok_or_else；R44 (P2) — execute_one_shot 中!writer.close().await? 改为 let _ = 忽略错误（one-shot 不复用 channel）；R45 (P2) — §7.2 N4 设计要点 ApprovalChannels 改为 ApprovalChannelManager；R47 (P2) — §8.1/§8.2 数据流 start_turn/build_one_shot_command 补充 attachments 参数 |
| v2.7.0 | 2026-10-07 | 第八轮评审修复（1 项）：R49 (P2) — setup_result 代码块中 writer.as_ref().unwrap() / reader.as_mut().unwrap() 改为 ok_or_else（与 R21/R42/R43 保持一致，防止 close() 在 step 1c→1d 间隙执行致 panic）。八轮评审累计修复 57 个问题，全部闭环，标记为评审通过 |
| v2.8.0 | 2026-10-07 | 第九轮评审 + openclaw 协议远程实测确认（3 项）：R50 (P0) — §6.3 OpenClawAdapter 全面重写，基于远程实测 v2026.9.8 确认实际协议：命令为 `openclaw agent exec "msg" --json`（非 `openclaw --server --json-rpc`），输出为一次性 JSON envelope（非 NDJSON 流式），one-shot 模式（multi_turn=false），无审批/中断/流式能力；R51 (P2) — §1.2 P0-1 明确 `--jsonl` 对 codex 不存在，openclaw 用 `--json`；R52 (P2) — §12 S9 "待协议确认"标注移除。§5.3 能力表 openclaw 列修正（approval/streaming/interrupt/multi_turn/compact 均为 ❌）。§12 实施计划重构为"先 openclaw（P0 one-shot）后 codex（P1 长连接）"方案，关键路径改为 S1→S3→S4→S5 |
| v2.9.0 | 2026-10-07 | P0 端到端测试通过 + P1 codex 实现完成（S6-S8）+ P1 端到端测试通过：openclaw 端到端测试 9/9 通过（CDP → create_ssh_agent → create_session → send_message → "Hello!" 响应）；codex v0.160.1 安装在远程，协议实测确认：`initialize` 需 `clientInfo` 字段，`thread/start` 响应 thread_id 在 `result.thread.id`，`turn/start` 用 `input: [{type:"text",text:"..."}]` 格式，无 `item/updated` 通知（改为 `item/agentMessage/delta`、`item/commandExecution/outputDelta` 等专用 delta 通知），`wire_api="chat"` 已移除（仅支持 `"responses"`）；S6 `json_rpc.rs`（JSON-RPC 编解码 + 5 单元测试），S7 `codex_adapter.rs`（CodexAppServerAdapter + 5 单元测试），S8 `agent.rs` 长连接模式（execute_long_connection + read_loop + interrupt）；codex 端到端测试 9/9 通过（CDP → create_ssh_agent → update_agent(model+tool_env_vars) → create_session → send_message → 通知接收：thinking/warning/error segments），全链路验证：SSH → codex app-server → initialize → thread/start → turn/start → 通知解析 → error 传播；`build_env_vars` 修复为从 `agent.tool_env_vars`（JSON 字符串）解析 HashMap 传递给 `exec_bidirectional` 前缀 `KEY=VAL command`；模型调用因 Agnes 仅支持 Chat Completions 而 codex 仅支持 Responses API 不兼容（`unknown variant 'namespace'` 错误），但长连接管道本身完全验证通过 |
| v2.10.0 | 2026-10-07 | 远程 codex-rs 编译部署方案（§14 新增）+ 两轮评审修复（17 项）：官方原版 codex CLI 不含本地 codex-rs 的 Responses API ↔ Chat Completions 适配器；决策：将本地 codex-rs + 适配器编译为 aarch64 Linux 二进制部署到远程（方案 A 单二进制 `codex-cloud-server`），新建独立 binary crate + 共享 crate `adapter-shared`，远程原生编译，启动时自动检测 API 类型并启动内置适配器；§12 新增 S12+S13；第一轮 8 项：R1 (P0) 交叉编译改为远程原生编译，R2 (P0) 注明 60+ 依赖范围，R3 (P1) 代码同步用共享 crate，R4 (P1) build_start_command 用 agent.command，R5 (P1) api_key 改为 --api-key 参数，R6 (P2) 参数兼容性注明，R7 (P2) 自动检测改为 Agent.command 配置，R8 (P1) 确认 adapter 自包含；第二轮 9 项：R9 (P1) stdio 占用澄清，R10 (P2) 完整依赖注明见 app-server/Cargo.toml，R11 (P1) 代码结构矛盾修复，R12 (P2) 上传改为 rsync，R13 (P1) 远程目录布局说明，R14 (P1) S12 描述明确 build_start_command 修改，R15 (P1) adapter-shared 重构后验证本地集成模式，R16 (P2) 验收标准补充本地回归测试，R17 (P1) S12 描述改为远程原生编译 |
| v2.11.0 | 2026-10-08 | S10 ClaudeCodeAdapter 实现完成：创建 `claude_code_adapter.rs`（MCP 模式 `claude mcp serve` JSON-RPC 2.0 + JSONL 降级 `claude -p --output-format json`）；MCP `initialize` 握手（protocolVersion 2024-11-05 + notifications/initialized）；`create_thread` 返回虚拟 thread_id `claude-mcp-default`；`start_turn` 发送 `tools/call`（claude_chat tool）；`parse_line` 解析 MCP 响应/请求/通知（tool_use_permission_request/command_permission_request 审批、notifications/message 错误、notifications/cancelled）；`parse_output` 解析 JSONL 事件流（assistant/tool_use/tool_result/result/error）；`build_env_vars` 提取 ANTHROPIC_API_KEY + ANTHROPIC_BASE_URL；工厂注册 `"claude-code" => ClaudeCodeAdapter::new()`（替换 OpenClawAdapter stub）；executor `execute()` 新增 MCP→JSONL 降级机制（长连接 setup 失败 + adapter 支持 one-shot → 自动降级）；22 单元测试全部通过，remote_protocol 全部 38 测试无回归 |
| v2.12.0 | 2026-10-08 | 工程重命名 `codex-cloud-server` → `botwork-cloud-server`（支持 codex + claude code 双远端）；adapter-shared 新增 `anthropic_converter.rs`（Anthropic Messages API ↔ OpenAI Chat Completions 转换：请求转换 system/messages/content blocks、响应转换 content/stop_reason/usage、流式 SSE 转换 message_start/content_block_delta/message_delta/message_stop）；adapter.rs 新增 `/v1/messages` 路由（handle_messages → anthropic_to_chat_completions → 转发 Chat Completions → chat_completions_to_anthropic 返回）；reqwest 启用 `stream` feature；14 单元测试全部通过 |
| v2.13.0 | 2026-10-08 | S11 claude code 端到端测试通过：修复 `build_one_shot_command` 参数（`--input-format json` 移除、`--permission-mode default` → `acceptEdits`、`--cwd` → `cd &&` 前缀）；修复 `ssh/manager.rs` `exec_bidirectional` 环境变量传递（`KEY=VAL cmd` 前缀 → `export KEY='VAL';` 语句，解决 `cd && claude` 时 env vars 仅作用于 `cd` 的问题）；anthropic_adapter.py 修复（强制 `stream=false` 上游 + SSE 流式响应转换 + 模型名映射 `claude-*` → `agnes-3.0-flash` + 请求日志）；E2E 测试 7/7 通过（CDP → create_ssh_agent → update_agent → create_session → send_message → "Hello!" 响应） |
| v2.14.0 | 2026-10-08 | 修复 claude code UI 乱码问题（用户报告发送中文消息出现 `<function=WebFetch>` 等工具调用语法泄漏到聊天界面）：1) `capabilities()` 改为 `multi_turn=false` + `protocol=Jsonl`，直接使用 one-shot 模式（`claude mcp serve` 不支持 `--model` 参数且 MCP 模式行为不适用于远程 agent 场景）；2) `build_one_shot_command` 添加 `--restricted` 标志禁用内置工具定义（防止模型看到工具定义后输出工具调用语法作为文本）；3) anthropic_adapter.py 添加 `do_HEAD` 健康检查支持 + 路径匹配改为 `"messages" in self.path`（处理 `/v1/v1/messages` 双重路径）；4) `ANTHROPIC_BASE_URL` 修正为不带 `/v1` 后缀（claude code 自动追加 `/v1/messages`）；中文消息 "当前使用的是什么模型？" 正确返回 "当前使用的模型是 Sonnet 4" 无乱码 |
| v2.15.0 | 2026-10-08 | anthropic_adapter.py 合并到 botwork-cloud-server（§15 新增）：删除独立 Python 适配器，将 Anthropic Messages API ↔ Chat Completions 转换逻辑统一到 Rust 内置适配器。P0 改动：1) `anthropic_converter.rs` 新增 `map_model()`（claude-* → agnes-* 模型名映射）+ `anthropic_response_to_sse_stream()`（非流式响应拆分为 SSE 事件序列，解决 Agnes 不支持流式但 claude code 发 stream=true 的问题）+ 6 单元测试；2) `adapter.rs` `ApiAdapter::start` 增加 `port: Option<u16>` 参数支持固定端口 + `/hello` `/health` HEAD 健康检查路由 + `handle_messages` 重写（模型映射在 chat_req 上做保持响应 model 为 claude-* + 强制 `stream=false` 上游）+ `proxy_anthropic_chat_response` 重写（上游始终非流式，客户端流式时拆分 JSON 为 SSE）；3) `main.rs` 新增 `--adapter-port` CLI 参数。P1 改动：4) 部署脚本 `common.sh` 删除 `deploy_adapter` + 新增 `configure_claude_code` + `start_cloud_server_service`（systemd/nohup 双模式启动 botwork-cloud-server 带 `--adapter-port` 参数）+ `configure_codex` base_url 改为指向适配器 `http://127.0.0.1:8082/v1`；5) 6 个 `deploy-*.sh` 同步更新；6) 删除 `anthropic_adapter.py`；7) README.md 更新。adapter-shared 20 单元测试全部通过 |
| v2.16.0 | 2026-10-09 | adapter 服务管理增强：`start_cloud_server_service` 新增 `service` 命令中间层（systemd → service → nohup 三级回退）；创建 `/etc/init.d/botwork-cloud-server` LSB init script（`start-stop-daemon --background` 启动 + `HOME=/root` 环境变量修复 `env -i` 下进程崩溃 + `update-rc.d defaults` 开机自启动）；`verify_install` 新增 service 状态检测；远端 192.168.1.7 验证 `service botwork-cloud-server start/stop/restart/status` 全部通过 |
