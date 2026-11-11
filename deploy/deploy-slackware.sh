#!/bin/bash
# deploy-slackware.sh — Slackware Linux 部署脚本 (slackpkg)
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

# ============ Slackware 专用：安装 Node.js ============
install_nodejs_distro() {
    # Slackware 15.0+ 自带 nodejs，但可能版本较旧
    if check_cmd node && check_cmd npm; then
        local node_ver
        node_ver=$(node --version 2>/dev/null | sed 's/v//' | cut -d. -f1)
        if [ "${node_ver:-0}" -ge "$NODE_MAJOR" ]; then
            log_info "Slackware 自带 Node.js 版本满足: $(node --version)"
            return 0
        fi
    fi

    # 使用 nvm 安装指定版本
    log_info "使用 nvm 安装 Node.js v${NODE_MAJOR}"
    slackpkg -batch=on install curl bash || true
    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash
    export NVM_DIR="$HOME/.nvm"
    source "$NVM_DIR/nvm.sh"
    nvm install "${NODE_MAJOR}"
    nvm use "${NODE_MAJOR}"
    nvm alias default "${NODE_MAJOR}"
}

# ============ Slackware 专用：安装系统依赖 ============
install_system_deps() {
    log_step "安装系统依赖 (Slackware)"

    # slackpkg 需要先更新镜像列表（如果首次使用）
    if [ ! -f /etc/slackpkg/mirrors ] || ! grep -v '^#' /etc/slackpkg/mirrors | grep -q .; then
        log_warn "slackpkg 镜像未配置，尝试自动配置"
        # 根据架构选择镜像
        local arch
        arch=$(uname -m)
        case "$arch" in
            x86_64)  echo "https://mirrors.slackware.com/slackware/slackware64-15.0/" > /etc/slackpkg/mirrors ;;
            *)       echo "https://mirrors.slackware.com/slackware/slackware-15.0/" > /etc/slackpkg/mirrors ;;
        esac
    fi

    slackpkg update

    # Slackware full install 已包含大部分开发工具
    # 补充可能缺失的包
    slackpkg -batch=on install \
        openssl \
        sqlite \
        sqlite3 \
        python3 \
        git \
        curl \
        cmake \
        perl || true

    # pip 安装
    if ! check_cmd pip3; then
        python3 -m ensurepip --default-pip 2>/dev/null || \
            slackpkg -batch=on install python-pip || true
    fi

    log_info "系统依赖安装完成"
}

# ============ 主流程 ============
main() {
    log_step "Bot Work 远程部署 — Slackware Linux"
    log_info "发行版: $(cat /etc/slackware-version 2>/dev/null || echo 'Unknown')"
    log_info "架构: $(uname -m)"
    log_info "Agnes API Key: ${AGNES_API_KEY:0:10}..."

    install_system_deps
    install_nodejs
    install_rust
    install_npm_tools
    configure_codex
    configure_claude_code

    if [ "$INSTALL_CLOUD_SERVER" = "yes" ]; then
        source "${SCRIPT_DIR}/deploy-cloud-server.sh"
        deploy_cloud_server
        start_cloud_server_service
    fi

    verify_install
}

main "$@"