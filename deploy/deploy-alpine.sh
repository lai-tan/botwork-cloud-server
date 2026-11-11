#!/bin/bash
# deploy-alpine.sh — Alpine Linux 部署脚本 (apk)
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

# ============ Alpine 专用：安装 Node.js ============
install_nodejs_distro() {
    # Alpine 3.21+ 自带 nodejs/npm，但版本可能较旧
    apk add --no-cache nodejs npm

    local node_ver
    node_ver=$(node --version 2>/dev/null | sed 's/v//' | cut -d. -f1)
    if [ "${node_ver:-0}" -ge "$NODE_MAJOR" ]; then
        log_info "Alpine 仓库 Node.js 版本满足要求: $(node --version)"
        return 0
    fi

    # 版本不足，使用 nvm 安装
    log_info "Alpine 仓库 Node.js 版本过低，使用 nvm 安装 v${NODE_MAJOR}"
    apk add --no-cache bash
    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash
    export NVM_DIR="$HOME/.nvm"
    source "$NVM_DIR/nvm.sh"
    nvm install "${NODE_MAJOR}"
    nvm use "${NODE_MAJOR}"
    nvm alias default "${NODE_MAJOR}"
}

# ============ Alpine 专用：安装系统依赖 ============
install_system_deps() {
    log_step "安装系统依赖 (Alpine Linux)"
    apk update --quiet
    apk add --no-cache \
        build-base \
        pkgconfig \
        openssl-dev \
        sqlite-dev \
        python3 \
        py3-pip \
        git \
        curl \
        cmake \
        bash \
        perl \
        linux-headers
    log_info "系统依赖安装完成"
}

# ============ 主流程 ============
main() {
    log_step "Bot Work 远程部署 — Alpine Linux"
    log_info "发行版: $(cat /etc/alpine-release 2>/dev/null || echo 'Unknown')"
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