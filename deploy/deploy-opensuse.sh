#!/bin/bash
# deploy-opensuse.sh — openSUSE 部署脚本 (zypper)
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

# ============ openSUSE 专用：安装 Node.js ============
install_nodejs_distro() {
    zypper --non-interactive --quiet install curl gnupg2

    curl -fsSL "https://rpm.nodesource.com/setup_${NODE_MAJOR}.x" | bash -
    zypper --non-interactive --quiet install nodejs

    if ! check_cmd npm; then
        zypper --non-interactive --quiet install npm
    fi
}

# ============ openSUSE 专用：安装系统依赖 ============
install_system_deps() {
    log_step "安装系统依赖 (openSUSE)"

    # 安装开发工具模式
    zypper --non-interactive --quiet install -t pattern devel_basis 2>/dev/null || \
        zypper --non-interactive --quiet install gcc gcc-c++ make

    zypper --non-interactive --quiet install \
        pkg-config \
        libopenssl-devel \
        sqlite3-devel \
        python3 \
        python3-pip \
        git \
        curl \
        cmake \
        perl
    log_info "系统依赖安装完成"
}

# ============ 主流程 ============
main() {
    log_step "Bot Work 远程部署 — openSUSE"
    log_info "发行版: $(rpm -q --qf '%{NAME} %{VERSION}' openSUSE-release 2>/dev/null || \
                        cat /etc/os-release 2>/dev/null | grep PRETTY_NAME | cut -d= -f2 || \
                        echo 'Unknown')"
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