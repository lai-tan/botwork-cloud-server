#!/bin/bash
# deploy.sh — Bot Work 远程一键部署主脚本
#
# 用法:
#   curl -fsSL <url>/deploy.sh | bash              # 自动检测发行版
#   AGNES_API_KEY=xxx bash deploy.sh               # 自定义 API Key
#   INSTALL_CLOUD_SERVER=no bash deploy.sh          # 跳过 cloud-server 编译
#   bash deploy.sh --skip-cloud-server              # 同上
#   bash deploy.sh --help                           # 帮助
#
# 支持的发行版:
#   - Ubuntu / Debian / Kali (apt)
#   - CentOS / RHEL / AlmaLinux / Rocky (yum/dnf)
#   - Fedora (dnf)
#   - Alpine Linux (apk)
#   - Arch Linux (pacman)
#   - openSUSE (zypper)
#   - Slackware (slackpkg)
#
# 环境变量:
#   AGNES_API_KEY        Agnes API Key (默认: 内置测试 key)
#   AGNES_MODEL          Agnes 模型名 (默认: agnes-3.0-flash)
#   NPM_REGISTRY         npm 镜像源 (默认: npmmirror)
#   NODE_MAJOR           Node.js 大版本号 (默认: 24)
#   ADAPTER_PORT         适配器端口 (默认: 8082)
#   INSTALL_CLOUD_SERVER 是否编译 botwork-cloud-server (默认: yes)

set -euo pipefail

# ============ 帮助 ============
show_help() {
    cat << 'EOF'
Bot Work 远程一键部署脚本

用法:
  bash deploy.sh [选项]

选项:
  --skip-cloud-server    跳过 botwork-cloud-server 编译
  --help                 显示帮助

环境变量:
  AGNES_API_KEY          Agnes API Key
  AGNES_MODEL            Agnes 模型名 (默认: agnes-3.0-flash)
  NPM_REGISTRY           npm 镜像源 (默认: npmmirror)
  NODE_MAJOR             Node.js 大版本号 (默认: 24)
  ADAPTER_PORT           适配器端口 (默认: 8082)
  INSTALL_CLOUD_SERVER   是否编译 cloud-server (yes/no, 默认: yes)

示例:
  bash deploy.sh
  AGNES_API_KEY=sk-xxx bash deploy.sh
  bash deploy.sh --skip-cloud-server
EOF
    exit 0
}

# ============ 参数解析 ============
while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-cloud-server)
            INSTALL_CLOUD_SERVER="no"
            shift
            ;;
        --help|-h)
            show_help
            ;;
        *)
            echo "未知选项: $1"
            show_help
            ;;
    esac
done

# ============ 检测发行版 ============
detect_distro() {
    # 方法 1: /etc/os-release (systemd 标准)
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        echo "$ID"
        return 0
    fi

    # 方法 2: /etc/lsb-release (Ubuntu/Debian)
    if [ -f /etc/lsb-release ]; then
        . /etc/lsb-release
        echo "${DISTRIB_ID:-ubuntu}"
        return 0
    fi

    # 方法 3: 包管理器检测
    if command -v apk >/dev/null 2>&1; then
        echo "alpine"
        return 0
    fi
    if command -v pacman >/dev/null 2>&1; then
        echo "arch"
        return 0
    fi
    if command -v zypper >/dev/null 2>&1; then
        echo "opensuse"
        return 0
    fi
    if command -v slackpkg >/dev/null 2>&1; then
        echo "slackware"
        return 0
    fi
    if command -v apt-get >/dev/null 2>&1; then
        echo "ubuntu"
        return 0
    fi
    if command -v dnf >/dev/null 2>&1; then
        echo "centos"
        return 0
    fi
    if command -v yum >/dev/null 2>&1; then
        echo "centos"
        return 0
    fi

    # 方法 4: rpm 检测
    if command -v rpm >/dev/null 2>&1; then
        if rpm -q centos-release >/dev/null 2>&1; then
            echo "centos"
            return 0
        fi
        if rpm -q redhat-release >/dev/null 2>&1; then
            echo "centos"
            return 0
        fi
        if rpm -q almalinux-release >/dev/null 2>&1; then
            echo "centos"
            return 0
        fi
        if rpm -q rocky-release >/dev/null 2>&1; then
            echo "centos"
            return 0
        fi
        if rpm -q fedora-release >/dev/null 2>&1; then
            echo "centos"
            return 0
        fi
        if rpm -q openSUSE-release >/dev/null 2>&1; then
            echo "opensuse"
            return 0
        fi
    fi

    # 方法 5: debian_version
    if [ -f /etc/debian_version ]; then
        echo "ubuntu"
        return 0
    fi

    # 方法 6: Slackware
    if [ -f /etc/slackware-version ]; then
        echo "slackware"
        return 0
    fi

    # 方法 7: Arch
    if [ -f /etc/arch-release ]; then
        echo "arch"
        return 0
    fi

    return 1
}

# ============ 主流程 ============
main() {
    echo ""
    echo "============================================"
    echo "  Bot Work 远程一键部署脚本"
    echo "  openclaw + codex + claude code"
    echo "============================================"
    echo ""

    # 检测发行版
    local distro
    if ! distro=$(detect_distro); then
        echo "[ERROR] 无法检测 Linux 发行版"
        echo "请手动指定:"
        echo "  bash deploy-ubuntu.sh      (Ubuntu/Debian/Kali)"
        echo "  bash deploy-centos.sh      (CentOS/RHEL/Fedora)"
        echo "  bash deploy-alpine.sh      (Alpine Linux)"
        echo "  bash deploy-arch.sh        (Arch Linux)"
        echo "  bash deploy-opensuse.sh    (openSUSE)"
        echo "  bash deploy-slackware.sh   (Slackware)"
        exit 1
    fi

    echo "[INFO] 检测到发行版: $distro"
    echo "[INFO] 架构: $(uname -m)"
    echo ""

    # 转换为小写
    distro=$(echo "$distro" | tr '[:upper:]' '[:lower:]')

    # 根据发行版调用对应脚本
    local script_dir
    script_dir="$(cd "$(dirname "$0")" && pwd)"

    case "$distro" in
        ubuntu|debian|linuxmint|pop|kali)
            bash "${script_dir}/deploy-ubuntu.sh"
            ;;
        centos|rhel|redhat|almalinux|rocky|fedora|amzn|ol)
            bash "${script_dir}/deploy-centos.sh"
            ;;
        alpine)
            bash "${script_dir}/deploy-alpine.sh"
            ;;
        arch|archlinux|manjaro|endeavouros)
            bash "${script_dir}/deploy-arch.sh"
            ;;
        opensuse|suse|sles|slemicro)
            bash "${script_dir}/deploy-opensuse.sh"
            ;;
        slackware|slackware64)
            bash "${script_dir}/deploy-slackware.sh"
            ;;
        *)
            echo "[WARN] 未知发行版: $distro"
            echo "尝试按包管理器检测..."
            if command -v apt-get >/dev/null 2>&1; then
                bash "${script_dir}/deploy-ubuntu.sh"
            elif command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1; then
                bash "${script_dir}/deploy-centos.sh"
            elif command -v apk >/dev/null 2>&1; then
                bash "${script_dir}/deploy-alpine.sh"
            elif command -v pacman >/dev/null 2>&1; then
                bash "${script_dir}/deploy-arch.sh"
            elif command -v zypper >/dev/null 2>&1; then
                bash "${script_dir}/deploy-opensuse.sh"
            elif command -v slackpkg >/dev/null 2>&1; then
                bash "${script_dir}/deploy-slackware.sh"
            else
                echo "[ERROR] 不支持的发行版: $distro"
                exit 1
            fi
            ;;
    esac
}

main "$@"