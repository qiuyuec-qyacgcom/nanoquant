#!/usr/bin/env bash
set -Eeuo pipefail

REPO="https://github.com/qiuyuec-qyacgcom/nanoquant"
BRANCH="${NANOQUANT_BRANCH:-main}"
INSTALL_DIR="./nanoquant"
ALLOW_STALE=0
FORWARD_ARGS=()

C_RESET='\033[0m'
C_OK='\033[32m'
C_WARN='\033[33m'
C_ERR='\033[31m'
C_INFO='\033[36m'
info() { printf "${C_INFO}[INFO]${C_RESET} %s\n" "$*"; }
ok() { printf "${C_OK}[OK]${C_RESET} %s\n" "$*"; }
warn() { printf "${C_WARN}[WARN]${C_RESET} %s\n" "$*"; }
die() { printf "${C_ERR}[ERROR]${C_RESET} %s\n" "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
用法: bash install.sh [引导选项] [install.sh 选项...]

引导选项:
  --dir <目录>          安装目录，默认 ./nanoquant
  --allow-stale         已存在仓库 git pull 失败时继续用旧代码
  -h, --help            显示帮助；代码已下载时转发 scripts/install.sh --help

其余参数透传给 scripts/install.sh，例如:
  --systemd --install-deps --password-stdin --port 8899 --no-start

仅支持 Linux。
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dir) [ -n "${2:-}" ] || die "--dir 需要非空值"; INSTALL_DIR="$2"; shift 2 ;;
    --dir=*) [ -n "${1#*=}" ] || die "--dir 需要非空值"; INSTALL_DIR="${1#*=}"; shift ;;
    --allow-stale) ALLOW_STALE=1; shift ;;
    -h|--help)
      if [ -f "$INSTALL_DIR/scripts/install.sh" ]; then
        exec bash "$INSTALL_DIR/scripts/install.sh" --help
      fi
      usage
      exit 0
      ;;
    *) FORWARD_ARGS+=("$1"); shift ;;
  esac
done

[ "$(uname -s)" = "Linux" ] || die "本项目仅支持 Linux"

detect_pm() {
  if command -v apt-get >/dev/null 2>&1; then echo apt
  elif command -v dnf >/dev/null 2>&1; then echo dnf
  elif command -v yum >/dev/null 2>&1; then echo yum
  elif command -v apk >/dev/null 2>&1; then echo apk
  elif command -v zypper >/dev/null 2>&1; then echo zypper
  elif command -v pacman >/dev/null 2>&1; then echo pacman
  else echo ""
  fi
}

install_fetch() {
  [ "$(id -u)" -eq 0 ] || die "自动安装 curl 需要 root；请手动安装 curl"
  local pm
  pm="$(detect_pm)"
  [ -n "$pm" ] || die "未找到 curl/wget，且无法识别包管理器；请手动安装 curl"
  info "安装 curl（$pm）"
  case "$pm" in
    apt) DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl ca-certificates ;;
    dnf) dnf install -y curl ca-certificates ;;
    yum) yum install -y curl ca-certificates ;;
    apk) apk add --no-cache curl ca-certificates ;;
    zypper) zypper --non-interactive install curl ca-certificates ;;
    pacman) pacman -Sy --needed --noconfirm curl ca-certificates ;;
  esac
}

FETCH=""
if command -v curl >/dev/null 2>&1; then
  FETCH="curl"
elif command -v wget >/dev/null 2>&1; then
  FETCH="wget"
else
  install_fetch
  FETCH="curl"
fi

fetch_url() {
  local url="$1"
  local output="$2"
  if [ "$FETCH" = "curl" ]; then
    curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$output"
  else
    wget --https-only -qO "$output" "$url"
  fi
}

TMP_DIR=""
cleanup() {
  [ -n "$TMP_DIR" ] && rm -rf "$TMP_DIR"
}
trap cleanup EXIT

if [ -d "$INSTALL_DIR/.git" ]; then
  info "更新现有仓库: $INSTALL_DIR"
  if ! git -C "$INSTALL_DIR" pull --ff-only; then
    if [ "$ALLOW_STALE" -eq 1 ]; then
      warn "git pull 失败，按 --allow-stale 继续使用现有代码"
    else
      die "git pull 失败；检查网络/分支冲突，或加 --allow-stale 使用现有代码"
    fi
  fi
elif [ -e "$INSTALL_DIR" ] && [ -n "$(ls -A "$INSTALL_DIR" 2>/dev/null)" ]; then
  die "目录已存在且不是 git 仓库: $INSTALL_DIR；请换 --dir 或清空后重试"
elif command -v git >/dev/null 2>&1; then
  info "git clone（depth 1）$REPO -> $INSTALL_DIR"
  git clone --depth 1 --branch "$BRANCH" "$REPO.git" "$INSTALL_DIR"
else
  info "未检测到 git，改用 tarball 下载"
  TMP_DIR="$(mktemp -d)"
  TARBALL="$TMP_DIR/nanoquant-$BRANCH.tar.gz"
  fetch_url "$REPO/archive/refs/heads/$BRANCH.tar.gz" "$TARBALL"
  if [ -n "${NANOQUANT_TARBALL_SHA256:-}" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "$TARBALL" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
      actual="$(shasum -a 256 "$TARBALL" | awk '{print $1}')"
    else
      die "设置了 NANOQUANT_TARBALL_SHA256 但缺少 sha256sum/shasum"
    fi
    [ "$actual" = "$NANOQUANT_TARBALL_SHA256" ] || die "tarball SHA256 不匹配: $actual"
    ok "tarball SHA256 校验通过"
  fi
  mkdir -p "$INSTALL_DIR"
  tar -xzf "$TARBALL" -C "$INSTALL_DIR" --strip-components=1
fi

[ -f "$INSTALL_DIR/scripts/install.sh" ] || die "下载内容缺少 scripts/install.sh"
cd "$INSTALL_DIR"
info "转交 scripts/install.sh 完成安装"
bash scripts/install.sh "${FORWARD_ARGS[@]+"${FORWARD_ARGS[@]}"}"