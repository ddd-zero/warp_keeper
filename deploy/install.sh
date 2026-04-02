#!/usr/bin/env bash
set -euo pipefail

PROGRAM="warp-keeper"
INSTALL_BIN_DIR="/usr/local/bin"
INSTALL_CONFIG_DIR="/etc/warp-keeper"
SYSTEMD_UNIT_PATH="/etc/systemd/system/warp-keeper.service"
OPENRC_UNIT_PATH="/etc/init.d/warp-keeper"
DEFAULT_REPO="ddd-zero/warp_keeper"

REPO="${KEEP_WARP_REPO:-}"
TAG="${KEEP_WARP_TAG:-}"
INSTALL_DAEMON=1
FORCE_AVX2=0
FORCE_BASELINE=0

usage() {
  cat <<'EOF'
用法:
  install.sh [--repo <owner/repo>] [--tag <tag>] [--force-avx2 | --force-baseline] [--no-daemon]

参数:
  --repo <owner/repo>  GitHub 仓库名（可选，默认 ddd-zero/warp_keeper）
  --tag <tag>          指定发布标签，不传则自动安装最新 release
  --no-daemon          只安装二进制，不自动注册守护进程
  --force-avx2         强制安装 AVX2 版本
  --force-baseline     强制安装基础兼容版本（不启用 AVX2）
  -h, --help           查看帮助
EOF
}

log() {
  printf '[INFO] %s\n' "$*"
}

warn() {
  printf '[WARN] %s\n' "$*" >&2
}

die() {
  printf '[ERROR] %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "缺少命令: $1"
}

validate_repo() {
  [[ "$1" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || die "仓库格式非法: $1"
}

validate_tag() {
  [[ "$1" =~ ^[A-Za-z0-9._/-]+$ ]] || die "标签格式非法: $1"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --repo)
        [[ $# -ge 2 ]] || die "--repo 缺少参数"
        REPO="$2"
        shift 2
        ;;
      --tag)
        [[ $# -ge 2 ]] || die "--tag 缺少参数"
        TAG="$2"
        shift 2
        ;;
      --force-avx2)
        FORCE_AVX2=1
        shift
        ;;
      --force-baseline)
        FORCE_BASELINE=1
        shift
        ;;
      --no-daemon)
        INSTALL_DAEMON=0
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        die "未知参数: $1"
        ;;
    esac
  done
}

ensure_root() {
  [[ "$(id -u)" -eq 0 ]] || die "请使用 root 运行，例如: curl ... | sudo bash"
}

resolve_repo_from_git() {
  if [[ -n "${REPO}" ]]; then
    return
  fi

  if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    local remote_url
    remote_url="$(git remote get-url origin 2>/dev/null || true)"
    if [[ "$remote_url" =~ github.com[:/]([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)(\.git)?$ ]]; then
      REPO="${BASH_REMATCH[1]}"
      log "未显式传入 --repo，已从 git remote 解析为: ${REPO}"
    fi
  fi

  if [[ -z "${REPO}" ]]; then
    REPO="${DEFAULT_REPO}"
    log "未显式传入 --repo，且未从 git remote 解析到仓库，使用默认仓库: ${REPO}"
  fi
}

require_linux() {
  local os
  os="$(uname -s)"
  [[ "${os}" == "Linux" ]] || die "当前系统为 ${os}，仅支持 Linux/Unix 兼容环境"
}

resolve_tag() {
  if [[ -n "${TAG}" ]]; then
    validate_tag "${TAG}"
    return
  fi

  local latest_api
  latest_api="https://api.github.com/repos/${REPO}/releases/latest"
  TAG="$(curl -fsSL "${latest_api}" | awk -F '"' '/"tag_name":/ {print $4; exit}')"
  [[ -n "${TAG}" ]] || die "无法获取最新 release 标签，请检查仓库是否已发布 release"
  validate_tag "${TAG}"
  log "自动选择最新标签: ${TAG}"
}

cpu_supports_avx2() {
  [[ -r /proc/cpuinfo ]] && grep -m1 -qi 'avx2' /proc/cpuinfo
}

pick_asset_name() {
  local arch
  arch="$(uname -m)"

  case "${arch}" in
    x86_64 | amd64)
      local baseline_asset="${PROGRAM}-linux-x86_64-musl"
      local avx2_asset="${PROGRAM}-linux-x86_64-musl-avx2"

      if [[ "${FORCE_AVX2}" -eq 1 && "${FORCE_BASELINE}" -eq 1 ]]; then
        die "--force-avx2 与 --force-baseline 不能同时使用"
      fi

      if [[ "${FORCE_AVX2}" -eq 1 ]]; then
        echo "${avx2_asset}"
        return
      fi

      if [[ "${FORCE_BASELINE}" -eq 1 ]]; then
        echo "${baseline_asset}"
        return
      fi

      if cpu_supports_avx2; then
        echo "${avx2_asset}"
      else
        echo "${baseline_asset}"
      fi
      ;;
    *)
      die "当前架构 ${arch} 暂未提供预编译包"
      ;;
  esac
}

asset_exists() {
  local asset_url="$1"
  curl -fsI "${asset_url}" >/dev/null 2>&1
}

download_asset() {
  local asset_name="$1"
  local temp_dir="$2"

  local asset_url="https://github.com/${REPO}/releases/download/${TAG}/${asset_name}"
  local sha_url="${asset_url}.sha256"

  if ! asset_exists "${asset_url}"; then
    local legacy_asset_url="${asset_url}.tar.xz"
    if asset_exists "${legacy_asset_url}"; then
      die "标签 ${TAG} 仍是旧的 tar.xz 发布格式，请发布新标签后重试，或手动安装 xz 再安装该旧标签"
    fi
    die "未找到发布产物: ${asset_name}"
  fi

  log "下载: ${asset_name}"
  curl -fL "${asset_url}" -o "${temp_dir}/${asset_name}"
  curl -fL "${sha_url}" -o "${temp_dir}/${asset_name}.sha256"

  # 为什么额外放一份到 dist/：兼容历史发布中的 sha256 文件写法（可能记录为 dist/<文件名>）。
  mkdir -p "${temp_dir}/dist"
  cp -f "${temp_dir}/${asset_name}" "${temp_dir}/dist/${asset_name}"

  (
    cd "${temp_dir}"
    sha256sum -c "${asset_name}.sha256"
  )
}

install_binary_and_assets() {
  local asset_name="$1"
  local temp_dir="$2"
  [[ -s "${temp_dir}/${asset_name}" ]] || die "下载文件为空: ${asset_name}"

  install -d "${INSTALL_BIN_DIR}" "${INSTALL_CONFIG_DIR}"
  install -m 0755 "${temp_dir}/${asset_name}" "${INSTALL_BIN_DIR}/${PROGRAM}"

  if [[ ! -f "${INSTALL_CONFIG_DIR}/config.toml" ]]; then
    local config_src="${temp_dir}/config.toml"
    local config_url="https://raw.githubusercontent.com/${REPO}/${TAG}/deploy/config.toml"
    log "首次安装，下载配置模板: ${INSTALL_CONFIG_DIR}/config.toml"
    curl -fL "${config_url}" -o "${config_src}" || die "下载配置模板失败: ${config_url}"
    install -m 0644 "${config_src}" "${INSTALL_CONFIG_DIR}/config.toml"
    log "执行客户端识别并写入 reconnect 配置"
    "${INSTALL_BIN_DIR}/${PROGRAM}" detect --config "${INSTALL_CONFIG_DIR}/config.toml"
  else
    # 为什么已有配置时不再执行 detect：升级安装必须保留用户手工调整的 toml，
    # 避免覆盖自定义 reconnect 配置或因环境瞬时状态导致探测结果回写。
    log "检测到已有配置，保留不覆盖且不重新执行 detect: ${INSTALL_CONFIG_DIR}/config.toml"
  fi
}

install_systemd_daemon() {
  local temp_dir="$1"
  local unit_src="${temp_dir}/warp-keeper.service"
  local unit_url="https://raw.githubusercontent.com/${REPO}/${TAG}/deploy/systemd/warp-keeper.service"
  curl -fL "${unit_url}" -o "${unit_src}" || return 1
  install -m 0644 "${unit_src}" "${SYSTEMD_UNIT_PATH}"
  systemctl daemon-reload
  systemctl enable --now warp-keeper.service
  log "已启用 systemd 守护: warp-keeper.service"
}

install_openrc_daemon() {
  local temp_dir="$1"
  local unit_src="${temp_dir}/warp-keeper.openrc"
  local unit_url="https://raw.githubusercontent.com/${REPO}/${TAG}/deploy/openrc/warp-keeper"
  curl -fL "${unit_url}" -o "${unit_src}" || return 1
  install -m 0755 "${unit_src}" "${OPENRC_UNIT_PATH}"
  rc-update add warp-keeper default >/dev/null 2>&1 || true
  rc-service warp-keeper restart >/dev/null 2>&1 || rc-service warp-keeper start
  log "已启用 OpenRC 守护: warp-keeper"
}

install_daemon_if_needed() {
  local temp_dir="$1"

  if [[ "${INSTALL_DAEMON}" -ne 1 ]]; then
    warn "按参数要求跳过守护安装，可手动执行 ${PROGRAM} run --config ${INSTALL_CONFIG_DIR}/config.toml"
    return
  fi

  # 为什么优先 systemd：Linux 主流发行版默认是 systemd，统一运维体验更稳定。
  if command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]]; then
    if ! install_systemd_daemon "${temp_dir}"; then
      warn "下载 systemd 守护模板失败，已完成二进制安装，请手动配置守护"
    fi
    return
  fi

  # 为什么提供 OpenRC：Alpine/Gentoo 常见，属于 Linux/Unix 场景中常用替代 init。
  if command -v rc-service >/dev/null 2>&1 && command -v rc-update >/dev/null 2>&1; then
    if ! install_openrc_daemon "${temp_dir}"; then
      warn "下载 OpenRC 守护模板失败，已完成二进制安装，请手动配置守护"
    fi
    return
  fi

  warn "未识别到 systemd/OpenRC，已完成二进制安装，请手动托管进程"
}

main() {
  parse_args "$@"
  ensure_root

  require_linux
  require_cmd curl
  require_cmd sha256sum

  resolve_repo_from_git
  validate_repo "${REPO}"

  resolve_tag

  local asset_name
  asset_name="$(pick_asset_name)"

  # 自动策略下，若 AVX2 包不存在，回退到基础版本，避免安装被中断。
  if [[ "${asset_name}" == "${PROGRAM}-linux-x86_64-musl-avx2" ]]; then
    local avx_url="https://github.com/${REPO}/releases/download/${TAG}/${asset_name}"
    if ! asset_exists "${avx_url}" && [[ "${FORCE_AVX2}" -eq 0 ]]; then
      warn "未找到 AVX2 包，回退到基础版本"
      asset_name="${PROGRAM}-linux-x86_64-musl"
    fi
  fi

  local temp_dir
  temp_dir="$(mktemp -d)"
  # 为什么使用默认值展开：避免 set -u 下脚本异常退出时 temp_dir 未绑定导致二次报错。
  trap 'rm -rf "${temp_dir:-}"' EXIT

  download_asset "${asset_name}" "${temp_dir}"
  install_binary_and_assets "${asset_name}" "${temp_dir}"
  install_daemon_if_needed "${temp_dir}"

  log "安装完成"
  log "配置文件: ${INSTALL_CONFIG_DIR}/config.toml"
  log "二进制路径: ${INSTALL_BIN_DIR}/${PROGRAM}"
}

# 为什么使用这个写法：兼容 `bash -s`（stdin 执行）+ `set -u`，避免 BASH_SOURCE 未绑定报错。
if [[ "${BASH_SOURCE[0]-$0}" == "$0" ]]; then
  main "$@"
fi
