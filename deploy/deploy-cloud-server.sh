#!/bin/bash
# deploy-cloud-server.sh — botwork-cloud-server 编译部署
# 被 deploy-ubuntu.sh / deploy-centos.sh source
set -euo pipefail

# ============ 部署 botwork-cloud-server ============
deploy_cloud_server() {
    log_step "部署 botwork-cloud-server"

    source "$HOME/.cargo/env" 2>/dev/null || true

    if ! check_cmd cargo; then
        log_error "cargo 未找到，请先安装 Rust"
        return 1
    fi

    if check_cmd botwork-cloud-server; then
        log_info "botwork-cloud-server 已安装: $(which botwork-cloud-server)"
        return 0
    fi

    local src_dir="${SCRIPT_DIR}/.."
    local remote_src="$HOME/botwork-cloud-server"

    # 检查源码是否已存在
    if [ -f "${remote_src}/Cargo.toml" ]; then
        log_info "源码已存在: ${remote_src}"
    elif [ -f "${src_dir}/Cargo.toml" ]; then
        log_info "从本地复制源码..."
        cp -r "$src_dir" "$remote_src"
    else
        log_warn "botwork-cloud-server 源码未找到，跳过编译"
        log_warn "请手动上传源码到 ${remote_src} 后运行:"
        log_warn "  cd ${remote_src} && cargo build --release && cp target/release/botwork-cloud-server /usr/local/bin/"
        return 0
    fi

    log_info "开始编译 botwork-cloud-server (可能需要 60-90 分钟)..."
    log_info "源码目录: ${remote_src}"

    cd "$remote_src"
    cargo build --release 2>&1 | tail -5

    local bin_path="${remote_src}/target/release/botwork-cloud-server"
    if [ -f "$bin_path" ]; then
        cp "$bin_path" /usr/local/bin/botwork-cloud-server
        chmod +x /usr/local/bin/botwork-cloud-server
        log_info "botwork-cloud-server 编译部署完成: $(ls -lh /usr/local/bin/botwork-cloud-server | awk '{print $5}')"
    else
        log_error "编译失败，二进制文件未找到: $bin_path"
        return 1
    fi
}