#!/bin/bash
# deploy-ubuntu.sh — Ubuntu/Debian 系部署脚本
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

# ============ Ubuntu/Debian 专用：安装 Node.js ============
install_nodejs_distro() {
    # 方法 1: NodeSource 官方仓库
    apt-get update -qq
    apt-get install -y -qq ca-certificates curl gnupg

    local keyring="/etc/apt/keyrings/nodesource.gpg"
    mkdir -p /etc/apt/keyrings
    curl -fsSL "https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key" | gpg --dearmor -o "$keyring"

    echo "deb [signed-by=$keyring] https://deb.nodesource.com/node_${NODE_MAJOR}.x nodistro main" > /etc/apt/sources.list.d/nodesource.list
    apt-get update -qq
    apt-get install -y -qq nodejs

    # 如果 npm 不存在，手动安装
    if ! check_cmd npm; then
        apt-get install -y -qq npm
    fi
}

# ============ Ubuntu/Debian 专用：安装系统依赖 ============
install_system_deps() {
    log_step "安装系统依赖 (Ubuntu/Debian)"
    apt-get update -qq
    apt-get install -y -qq \
        build-essential \
        pkg-config \
        libssl-dev \
        libsqlite3-dev \
        python3 \
        python3-pip \
        git \
        curl \
        cmake
    log_info "系统依赖安装完成"
}

# ============ 主流程 ============
main() {
    log_step "Bot Work 远程部署 — Ubuntu/Debian"
    log_info "发行版: $(cat /etc/lsb-release 2>/dev/null | grep DISTRIB_DESCRIPTION | cut -d= -f2 || echo 'Unknown')"
    log_info "架构: $(uname -m)"


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