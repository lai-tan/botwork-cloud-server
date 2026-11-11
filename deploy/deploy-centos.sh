#!/bin/bash
# deploy-centos.sh — CentOS/RHEL/Fedora/AlmaLinux/Rocky 系部署脚本
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

# ============ CentOS/RHEL 专用：安装 Node.js ============
install_nodejs_distro() {
    local pkg_mgr
    if check_cmd dnf; then
        pkg_mgr="dnf"
    else
        pkg_mgr="yum"
    fi

    # 方法 1: NodeSource 官方仓库
    $pkg_mgr install -y -q curl gnupg2

    local major
    major=$(rpm -E %{rhel} 2>/dev/null || echo "9")

    curl -fsSL "https://rpm.nodesource.com/setup_${NODE_MAJOR}.x" | bash -

    $pkg_mgr install -y -q nodejs

    # 如果 npm 不存在，手动安装
    if ! check_cmd npm; then
        $pkg_mgr install -y -q npm
    fi
}

# ============ CentOS/RHEL 专用：安装系统依赖 ============
install_system_deps() {
    log_step "安装系统依赖 (CentOS/RHEL)"
    local pkg_mgr
    if check_cmd dnf; then
        pkg_mgr="dnf"
    else
        pkg_mgr="yum"
    fi

    # 启用 EPEL (CentOS/RHEL 需要)
    if ! rpm -q epel-release >/dev/null 2>&1; then
        $pkg_mgr install -y -q epel-release 2>/dev/null || true
    fi

    $pkg_mgr groupinstall -y -q "Development Tools" 2>/dev/null || \
        $pkg_mgr install -y -q gcc gcc-c++ make

    $pkg_mgr install -y -q \
        pkgconfig \
        openssl-devel \
        sqlite-devel \
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
    log_step "Bot Work 远程部署 — CentOS/RHEL"
    log_info "发行版: $(rpm -q --qf '%{NAME} %{VERSION}' centos-release 2>/dev/null || \
                        rpm -q --qf '%{NAME} %{VERSION}' redhat-release 2>/dev/null || \
                        rpm -q --qf '%{NAME} %{VERSION}' almalinux-release 2>/dev/null || \
                        rpm -q --qf '%{NAME} %{VERSION}' rocky-release 2>/dev/null || \
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