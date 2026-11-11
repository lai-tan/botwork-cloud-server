# botwork-cloud-server

云端 AI 服务器 — 集成 Responses API ↔ Chat Completions 适配器 + Anthropic Messages API ↔ Chat Completions 适配器，编译为 aarch64 Linux 二进制部署到远程机器。

支持两种远端：
- **codex**：通过 Responses API ↔ Chat Completions 适配器
- **claude code**：通过 Anthropic Messages API ↔ Chat Completions 适配器

## 架构

```
bot-work-windows (Windows)                远程机器 (aarch64 Linux)
┌──────────────────────┐                 ┌──────────────────────────────────────┐
│  SshAgentExecutor    │                 │  botwork-cloud-server (单二进制)      │
│  ├── SSH 连接 ──────────────────────→ │  ├── 内置 ApiAdapter (axum 代理)      │
│  │   JSON-RPC over stdio              │  │   ├── detect_api_type()            │
│  │   ← 通知/响应                       │  │   ├── Responses ↔ Chat 转换        │
│  └── ProtocolAdapter                 │  │   └── Anthropic Messages ↔ Chat 转换│
│      ├── CodexAppServerAdapter       │  └── codex app-server (codex-rs)      │
│      └── ClaudeCodeAdapter           │      └── base_url → 内置适配器        │
└──────────────────────┘                 └──────────────────────────────────────┘
                                                    ↓
                                         Agnes API (apihub.agnes-ai.com)
```

## 工程结构

```
botwork-cloud-server/
├── Cargo.toml              # workspace 根
├── adapter-shared/         # 共享适配器 crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── adapter.rs              # axum HTTP 代理
│       ├── converter.rs            # Responses ↔ Chat Completions 格式转换
│       ├── anthropic_converter.rs  # Anthropic Messages ↔ Chat Completions 格式转换
│       └── api_detect.rs          # API 类型检测
└── server/                 # botwork-cloud-server binary crate
    ├── Cargo.toml
    └── src/
        └── main.rs         # 入口
```

## 编译（远程原生编译）

远程机器环境：Android 内核 + Ubuntu userspace (GLIBC 2.39) + GCC 13.3 + 7 核 + 22GB RAM

```bash
# 1. 远程安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 2. rsync 同步源码到远程
rsync -avz --exclude target botwork-cloud-server/ root@remote:/root/botwork-cloud-server/
rsync -avz --exclude target bot-work-windows/vendor/codex/codex-rs/ root@remote:/root/bot-work-windows/vendor/codex/codex-rs/
rsync -avz --exclude target botwork-cloud-server/adapter-shared/ root@remote:/root/botwork-cloud-server/adapter-shared/

# 3. 远程编译
cd /root/botwork-cloud-server
cargo build --release

# 4. 部署
cp target/release/botwork-cloud-server /usr/local/bin/
```

## 使用

```bash
# 启动（与 codex app-server 兼容的参数 + --api-key）
botwork-cloud-server --listen stdio:// \
  -c model="agnes-3.0-flash" \
  -c model_providers.openai.base_url="https://apihub.agnes-ai.com/v1" \
  --api-key="sk-xxx"
```

## 依赖

- codex-rs（vendored 在 `../bot-work-windows/vendor/codex/codex-rs/`）
- adapter-shared（本 workspace 内）
- axum 0.8 + reqwest 0.12 + tokio 1
