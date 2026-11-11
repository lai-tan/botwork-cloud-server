# Bot Work 远程一键部署

云端重装系统后，运行一条命令即可部署 openclaw + codex + claude code + botwork-cloud-server（内置 Anthropic 适配器）。

## 快速开始

```bash
# 1. 上传 deploy/ 目录到远程机器
scp -r deploy/ root@<host>:/root/

# 2. SSH 到远程机器
ssh root@<host>

# 3. 运行部署脚本
cd /root/deploy && bash deploy.sh
```

## 自定义配置

```bash
# 自定义 Agnes API Key
AGNES_API_KEY=sk-xxx bash deploy.sh

# 跳过 botwork-cloud-server 编译（节省 60-90 分钟）
bash deploy.sh --skip-cloud-server

# 自定义 Node.js 版本
NODE_MAJOR=22 bash deploy.sh

# 使用官方 npm 源（海外机器）
NPM_REGISTRY=https://registry.npmjs.org bash deploy.sh
```

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `AGNES_API_KEY` | 内置测试 key | Agnes API Key |
| `AGNES_MODEL` | `agnes-3.0-flash` | Agnes 模型名 |
| `NPM_REGISTRY` | `https://registry.npmmirror.com` | npm 镜像源 |
| `NODE_MAJOR` | `24` | Node.js 大版本号 |
| `ADAPTER_PORT` | `8082` | botwork-cloud-server 内置适配器端口 |
| `CLOUD_SERVER_PORT` | `8081` | botwork-cloud-server WebSocket 监听端口 |
| `INSTALL_CLOUD_SERVER` | `yes` | 是否编译部署 botwork-cloud-server |

## 支持的发行版

| 发行版 | 包管理器 | 脚本 |
|--------|----------|------|
| Ubuntu 20.04+ | apt-get | `deploy-ubuntu.sh` |
| Debian 11+ | apt-get | `deploy-ubuntu.sh` |
| Kali Linux | apt-get | `deploy-ubuntu.sh` |
| CentOS Stream 9/10 | yum/dnf | `deploy-centos.sh` |
| RHEL 8+ | yum/dnf | `deploy-centos.sh` |
| AlmaLinux 8+ | dnf | `deploy-centos.sh` |
| Rocky Linux 8+ | dnf | `deploy-centos.sh` |
| Fedora 41/42 | dnf | `deploy-centos.sh` |
| Alpine 3.21/latest | apk | `deploy-alpine.sh` |
| Arch Linux | pacman | `deploy-arch.sh` |
| openSUSE | zypper | `deploy-opensuse.sh` |
| Slackware 15.0+ | slackpkg | `deploy-slackware.sh` |

## 部署内容

1. **系统依赖**: build-essential/gcc, openssl-dev, sqlite-dev, python3, git, cmake
2. **Node.js**: v24 (NodeSource 官方仓库)
3. **Rust**: stable (rustup)
4. **openclaw**: npm 全局安装
5. **codex**: npm 全局安装 (@openai/codex)
6. **claude code**: npm 全局安装 (@anthropic-ai/claude-code)
7. **codex 配置**: `~/.codex/config.toml` (base_url 指向内置适配器)
8. **botwork-cloud-server**: Rust 编译部署 + 后台服务启动 (~60-90 分钟)
   - 内置适配器: `/v1/responses` (服务 codex) + `/v1/messages` (服务 claude code)
   - 模型映射: `claude-*` → `agnes-*`
   - SSE 伪流式: 上游非流式 → 客户端流式

## 文件结构

```
deploy/
├── deploy.sh                 # 主入口 (自动检测发行版)
├── common.sh                 # 共享函数库
├── deploy-ubuntu.sh          # Ubuntu/Debian/Kali 部署 (apt-get)
├── deploy-centos.sh          # CentOS/RHEL/Fedora 部署 (yum/dnf)
├── deploy-alpine.sh          # Alpine Linux 部署 (apk)
├── deploy-arch.sh            # Arch Linux 部署 (pacman)
├── deploy-opensuse.sh        # openSUSE 部署 (zypper)
├── deploy-slackware.sh       # Slackware 部署 (slackpkg)
├── deploy-cloud-server.sh    # botwork-cloud-server 编译

└── README.md                 # 本文档
```

## 验证

部署完成后，脚本会自动验证所有组件：

```
[INFO]  openclaw: OpenClaw 2026.9.8
[INFO]  codex: codex-cli 0.160.1
[INFO]  claude: 2.1.293 (Claude Code)
[INFO]  node: v24.21.0
[INFO]  python3: Python 3.12.3
[INFO]  codex 配置: OK
[INFO]  botwork-cloud-server 服务: 运行中
[INFO]  botwork-cloud-server: 0.1.0
```

## 故障排查

```bash
# 查看 botwork-cloud-server 日志 (systemd 环境)
journalctl -u botwork-cloud-server -e

# 查看 botwork-cloud-server 日志 (无 systemd: Alpine/Slackware/容器)
cat /tmp/botwork-cloud-server.log

# 重启 botwork-cloud-server (systemd)
systemctl restart botwork-cloud-server

# 重启 botwork-cloud-server (无 systemd)
pkill -f botwork-cloud-server
# 然后重新运行部署脚本，或手动启动：
AGNES_API_KEY=sk-xxx nohup botwork-cloud-server \
  --listen ws://127.0.0.1:8081 \
  --api-key sk-xxx \
  --base-url https://apihub.agnes-ai.com/v1 \
  --adapter-port 8082 \
  > /tmp/botwork-cloud-server.log 2>&1 &

# 测试适配器 /v1/messages (claude code 端点)
curl -s -X POST http://127.0.0.1:8082/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: $AGNES_API_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -d '{"model":"claude-sonnet-4-20250514","max_tokens":50,"messages":[{"role":"user","content":"Hi"}]}'

# 测试适配器 /v1/responses (codex 端点)
curl -s -X POST http://127.0.0.1:8082/v1/responses \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $AGNES_API_KEY" \
  -d '{"model":"agnes-3.0-flash","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Hi"}]}]}'

# 健康检查
curl -s http://127.0.0.1:8082/hello

# 检查 codex 配置
cat ~/.codex/config.toml
```

### 无 systemd 环境

Alpine Linux、Slackware 和容器/chroot 环境没有 systemd，部署脚本会自动回退到 `nohup` 方式启动 botwork-cloud-server。

| 环境 | init 系统 | 启动方式 |
|------|-----------|----------|
| Ubuntu/Debian/CentOS/Fedora/Arch/openSUSE | systemd | systemd service |
| Alpine Linux | OpenRC | nohup |
| Slackware | BSD init | nohup |
| 容器/chroot | 无 | nohup |