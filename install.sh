#!/usr/bin/env bash
# ============================================================
# NanoQuant 在线安装引导脚本（bootstrap）
#
# 一条命令安装，无需先 git clone：
#
#   if [ -f /usr/bin/curl ];then curl -sSO https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh;else wget -O install.sh https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh;fi;bash install.sh --password '你的密码'
#
# 本脚本只负责「下载完整代码」+「转交 scripts/install.sh」；
# 依赖安装、目录准备、systemd 注册等都在 scripts/install.sh 里完成（幂等，可反复执行）。
#
# 额外参数：
#   --dir <目录>   安装目录，默认 ./nanoquant（其余参数全部透传给 scripts/install.sh）
# ============================================================
set -euo pipefail

REPO="https://github.com/qiuyuec-qyacgcom/nanoquant"
BRANCH="${NANOQUANT_BRANCH:-main}"
INSTALL_DIR=""

C_RESET='\033[0m'; C_OK='\033[32m'; C_WARN='\033[33m'; C_ERR='\033[31m'; C_INFO='\033[36m'
info() { printf "${C_INFO}[信息]${C_RESET} %s\n" "$*"; }
ok()   { printf "${C_OK}[完成]${C_RESET} %s\n" "$*"; }
warn() { printf "${C_WARN}[注意]${C_RESET} %s\n" "$*"; }
die()  { printf "${C_ERR}[失败]${C_RESET} %s\n" "$*" >&2; exit 1; }

# 抽出 --dir，其余透传给 scripts/install.sh
FORWARD_ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dir) INSTALL_DIR="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) FORWARD_ARGS+=("$1"); shift ;;
  esac
done

INSTALL_DIR="${INSTALL_DIR:-./nanoquant}"

# ---------- 下载工具兜底（curl 优先，wget 次之；都没有则自动装 curl）----------
if command -v curl >/dev/null 2>&1; then
  FETCH="curl"
elif command -v wget >/dev/null 2>&1; then
  FETCH="wget"
else
  warn "未检测到 curl / wget，尝试自动安装 curl"
  if command -v apt-get >/dev/null 2>&1; then
    DEBIAN_FRONTEND=noninteractive apt-get update -qq || true
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl || die "自动安装 curl 失败，请手动: apt-get install -y curl"
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y -q curl || die "自动安装 curl 失败，请手动: dnf install -y curl"
  elif command -v yum >/dev/null 2>&1; then
    yum install -y -q curl || die "自动安装 curl 失败，请手动: yum install -y curl"
  elif command -v apk >/dev/null 2>&1; then
    apk add --no-cache curl || die "自动安装 curl 失败，请手动: apk add curl"
  else
    die "未找到 curl 或 wget，且无法识别包管理器，请手动安装 curl"
  fi
  FETCH="curl"
fi
ok "下载工具：$FETCH"

# ---------- 获取代码（git 优先；无 git 用 tarball）----------
if [[ -d "$INSTALL_DIR/.git" ]]; then
  info "目录 $INSTALL_DIR 已是仓库，git pull 更新"
  git -C "$INSTALL_DIR" pull --ff-only || warn "更新失败，继续使用现有代码"
elif command -v git >/dev/null 2>&1; then
  info "git clone（depth 1）$REPO -> $INSTALL_DIR"
  git clone --depth 1 --branch "$BRANCH" "$REPO.git" "$INSTALL_DIR"
else
  info "未检测到 git，改用 tarball 下载"
  TARBALL_URL="$REPO/archive/refs/heads/$BRANCH.tar.gz"
  TMP_TAR="$(mktemp -d)/nanoquant-$BRANCH.tar.gz"
  if [[ "$FETCH" == "curl" ]]; then
    curl -fsSL "$TARBALL_URL" -o "$TMP_TAR" || die "下载失败：$TARBALL_URL"
  else
    wget -qO "$TMP_TAR" "$TARBALL_URL" || die "下载失败：$TARBALL_URL"
  fi
  mkdir -p "$INSTALL_DIR"
  tar -xzf "$TMP_TAR" -C "$INSTALL_DIR" --strip-components=1
  rm -f "$TMP_TAR"
fi

# ---------- 转交正式安装脚本 ----------
cd "$INSTALL_DIR"
info "转交 scripts/install.sh 完成安装"
# 兼容老 bash（<4.4 下空数组 + set -u 会报 unbound）
bash scripts/install.sh "${FORWARD_ARGS[@]+"${FORWARD_ARGS[@]}"}"
