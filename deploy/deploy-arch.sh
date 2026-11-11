#!/bin/bash
# deploy-arch.sh — Arch Linux 部署脚本 (pacman)
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

# ============ Arch 专用：安装 Node.js ============
install_nodejs_distro() {
    # Arch 官方仓库自带最新 nodejs/npm
    pacman -S --noconfirm --needed nodejs npm
}

# ============ Arch 专用：安装系统依赖 ============
install_system_deps() {
    log_step "安装系统依赖 (Arch Linux)"

    # 初始化 keyring（如果是新安装）
    if ! pacman -Q archlinux-keyring >/dev/null 2>&1; then
        pacman -S --noconfirm --needed archlinux-keyring
    fi

    pacman -S --noconfirm --needed \
        base-devel \
        pkgconf \
        openssl \
        sqlite \
        python \
        python-pip \
        git \
        curl \
        cmake \
        perl
    log_info "系统依赖安装完成"
}

# ============ 主流程 ============
main() {
    log_step "Bot Work 远程部署 — Arch Linux"
    log_info "发行版: Arch Linux"
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