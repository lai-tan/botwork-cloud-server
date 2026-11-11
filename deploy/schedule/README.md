# 调度守护进程部署（步骤 23b：三级部署脚本）

AUTOMATION-DESIGN.md §5.2 部署三级回退 + §5.2.1 卡死检测 + §9 步骤 23b 的落地实现。

## 文件清单

| 文件 | 用途 |
|------|------|
| `install-schedule-daemon.sh` | 统一安装器：三级检测（systemd → SysV init/service → nohup）+ 配置生成 + 端口冲突迁移 + 安装验证 |
| `botwork-cloud-schedule.init` | SysV init 脚本（优先级 2；目标 chroot 环境即此级别） |
| `botwork-cloud-schedule.service` | systemd unit 模板（优先级 1；install 脚本会按 env 生成同内容 unit） |
| `watchdog.sh` | 看门狗（SysV/nohup 级配套；`/health` 30s 轮询，连续 3 次失败 TERM→重启） |
| `schedule-daemon.env.example` | 配置模板（API key、端口、并发参数等） |
| `verify-schedule-daemon.sh` | 端到端验证（健康检查 → POST once 任务真实触发 → run 记录 → DELETE 清理） |

## 部署流程（远程机执行）

```bash
# 1. 编译安装二进制（含 --schedule 的源码；上传方式见仓库根 upload_to_remote.py）
cd /root/botwork-cloud-server && cargo build --release
cp target/release/botwork-cloud-server /usr/local/bin/

# 2. 安装调度守护进程（三级回退自动检测）
cd /root/botwork-cloud-server/deploy/schedule
SCHEDULE_API_KEY=sk-... ./install-schedule-daemon.sh

# 3. 端到端验证（真实触发一次 once 任务，调 Agnes API 一次）
./verify-schedule-daemon.sh
```

## 端口说明（重要）

- 管理 API 默认 **8081**（仅绑定 127.0.0.1）——bot-work-windows 前端 SSH 同步协议
  （`commands/schedule.rs` 的 `remote_admin_request`）写死 8081，**不要改**。
- 既有 `botwork-cloud-server --listen ws://127.0.0.1:8081` 常驻服务与此端口冲突：
  install 脚本自动将其迁移为 `--listen off`（释放 8081），**adapter 8082 保留**
  （远程 `~/.codex/config.toml` 指向 8082，codex 执行链路不受影响）。
- 调度进程不传 `--base-url`（不启进程内 adapter），codex thread 走远程既有
  config.toml → 8082 adapter → Agnes。

## 部署级别差异

| 级别 | 检测条件 | 崩溃自恢复 | 卡死检测 | 停机命令 |
|------|----------|------------|----------|----------|
| systemd | `systemctl` 可用且 `/run/systemd/system` 存在 | `Restart=always` + 5s | （WatchdogSec 为可选增强，未启用） | `systemctl stop botwork-cloud-schedule`（TimeoutStopSec=35） |
| SysV/service | `service` 可用 | watchdog.sh 30s 轮询 | 同左（/health 心跳） | `service botwork-cloud-schedule stop`（先杀 watchdog，TERM 等 35s） |
| nohup | 以上均无 | 同 watchdog（拉起命令为 start-schedule.sh） | 同左 | `kill $(cat /run/botwork-cloud-schedule.pid)` |

## 历史教训（脚本内已规避）

- init 脚本内 `start-stop-daemon` 不做输出重定向（曾因此启动失败）；日志由进程内
  tracing-appender 写 `/root/bot-work/logs/schedule-YYYYMMDD.log`。
- `export HOME=/root` 必须显式设置（chroot 缺 HOME 会启动即崩）。
- `service` 命令存在不代表正常工作：部署时先直接运行 init 脚本验证，再测 service 包装层。
- watchdog 停机顺序：stop 先杀 watchdog 再停服务（否则 stop 被视为崩溃而重新拉起）。
- watchdog 日志 `/root/bot-work/watchdog.log` 约 200KB/天，定期清理或配 logrotate。

## chroot 环境重启注意

chroot 内服务不随宿主机自启：设备重启后须手动恢复（`service botwork-cloud-server start`
恢复 adapter，`service botwork-cloud-schedule start` 恢复调度守护 + watchdog）。