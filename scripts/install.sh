#!/usr/bin/env bash
set -Eeuo pipefail
umask 022

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/rust/nanoquant/target/release/nanoquant"
FRONTEND_DIST="$ROOT/frontend/dist"
FRONTEND_INDEX="$FRONTEND_DIST/index.html"
DEFAULT_PORT=8899
MIN_NODE_MAJOR="${NANOQUANT_MIN_NODE_MAJOR:-20}"
MIN_FREE_MB="${NANOQUANT_MIN_FREE_MB:-2048}"
PORT="${NANOQUANT_PORT:-$DEFAULT_PORT}"
PORT_EXPLICIT=0
ADMIN_PASSWORD="${NANOQUANT_ADMIN_PASSWORD:-}"
PASSWORD_FLAG=0
PASSWORD_GENERATED=0
SYSTEMD_MODE="auto"
SYSTEMD=0
RUN_USER="${NANOQUANT_USER:-}"
RUN_GROUP=""
CREATED_USER=0
NO_FRONTEND=0
FORCE_FRONTEND=0
SKIP_BUILD=0
SKIP_DEPS=0
INSTALL_DEPS=0
INSTALL_RUST=0
INSTALL_NODE=0
AUTO_DEPS="${NANOQUANT_AUTO_DEPS:-1}"
case "$AUTO_DEPS" in
  1|true|TRUE|yes|YES|on|ON) AUTO_DEPS=1 ;;
  *) AUTO_DEPS=0 ;;
esac
SUDO=""
SYSTEM_DEPS_DONE=0
SKIP_DISK_CHECK=0
CHECK_ONLY=0
OFFLINE=0
NO_START=0
NO_RESTART=0
LOG_DIR="$ROOT/logs"
LOG_FILE=""
ENV_FILE=""
ENV_FILE_CREATED=0
BACKUP_BIN=""
BACKUP_UNIT=""
PM=""
RUST_MIN=""
STEP=""

C_RESET=""
C_OK=""
C_WARN=""
C_ERR=""
C_INFO=""
if [ -t 1 ]; then
  C_RESET=$'\033[0m'
  C_OK=$'\033[32m'
  C_WARN=$'\033[33m'
  C_ERR=$'\033[31m'
  C_INFO=$'\033[36m'
fi

usage() {
  cat <<'EOF'
用法: bash scripts/install.sh [选项]

Linux 专用安装脚本：构建并就地安装 NanoQuant Rust 内核与前端。

运行模式:
  --systemd             强制 systemd；自动创建 nanoquant 系统用户
  --no-systemd          强制 nohup；使用 scripts/start.sh --daemon
  默认                   root 且 systemd 可用时自动 systemd，否则 nohup

安装内容:
  默认                   自动补齐 gcc/make/perl/git/curl/Rust/Node 等缺失工具
  --install-deps        兼容旧参数；默认已自动安装
  --install-rust        兼容旧参数；默认已自动安装
  --install-node        兼容旧参数；默认已自动安装
  --no-auto-deps        关闭自动安装；缺失依赖时直接报错
  --offline             cargo/npm 使用离线模式（依赖需已缓存，且不联网装工具）
  --skip-deps           跳过工具链检查
  --skip-disk-check     跳过磁盘空间检查

构建与产物:
  --password <密码>     管理员初始密码；会进入 shell 历史，建议用 stdin/env
  --password-stdin      从 stdin 读取管理员初始密码
  --password-file <文件> 从文件首行读取管理员初始密码
  未提供密码        自动生成 20 位初始密码，安装成功后打印一次
  --port <端口>         覆盖端口；默认保留已有 data/.port，否则 8899
  --no-frontend         只处理后端；要求已有 frontend/dist/index.html
  --force-frontend      即使 dist 已存在也重建前端
  --skip-build          复用已编译的 Rust 二进制
  --user <系统用户>     systemd 运行用户；需已存在，默认自动创建 nanoquant

启动与检查:
  --no-start            只安装，不启动服务
  --no-restart          不重启已运行服务（新二进制不会生效）
  --check               只检查环境与产物，不修改文件
  -h, --help            显示帮助

示例:
  sudo bash scripts/install.sh --systemd
  NANOQUANT_ADMIN_PASSWORD='***' sudo -E bash scripts/install.sh --systemd
  sudo bash scripts/install.sh --password-stdin --systemd < /root/nq-password
  bash scripts/install.sh --check
EOF
}

log() {
  local level="$1"
  shift
  local color=""
  case "$level" in
    INFO) color="$C_INFO" ;;
    OK) color="$C_OK" ;;
    WARN) color="$C_WARN" ;;
    ERROR) color="$C_ERR" ;;
  esac
  printf '%s[%s]%s %s\n' "$color" "$level" "$C_RESET" "$*" >&2
  if [ -n "$LOG_FILE" ]; then
    printf '[%s] %s\n' "$level" "$*" >>"$LOG_FILE"
  fi
}

die() {
  log ERROR "$*"
  exit 1
}

run_logged() {
  if [ -n "$LOG_FILE" ]; then
    "$@" 2>&1 | tee -a "$LOG_FILE"
  else
    "$@"
  fi
}

run_root_logged() {
  if [ "$(id -u)" -eq 0 ]; then
    run_logged "$@"
  elif [ -n "$SUDO" ]; then
    run_logged "$SUDO" "$@"
  else
    die "自动安装系统依赖需要 root 或 sudo；请用 sudo 运行安装脚本"
  fi
}

cleanup_password() {
  if [ "$ENV_FILE_CREATED" -eq 1 ] && [ -n "$ENV_FILE" ] && [ -f "$ENV_FILE" ]; then
    rm -f "$ENV_FILE" || true
  fi
}

generate_admin_password() {
  local password=""
  if command -v openssl >/dev/null 2>&1; then
    password="$(openssl rand -base64 32 2>/dev/null | tr -d '\n/+=' | cut -c1-20 || true)"
  fi
  if [ -z "$password" ]; then
    password="$(od -An -N16 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n' | cut -c1-20 || true)"
  fi
  [ "${#password}" -ge 16 ] || die "无法生成管理员密码；请用 --password 手动指定"
  printf '%s\n' "$password"
}

admin_password_state() {
  [ -x "$BIN" ] || { printf 'unknown\n'; return; }
  local db="$ROOT/data/nanoquant.db"
  [ -f "$db" ] || { printf 'missing\n'; return; }
  local output
  output="$("$BIN" ops preflight --db "$db" 2>/dev/null || true)"
  case "$output" in
    *"密码哈希存在"*) printf 'present\n' ;;
    *"缺少密码哈希"*) printf 'missing\n' ;;
    *) printf 'unknown\n' ;;
  esac
}

print_generated_password() {
  [ "$PASSWORD_GENERATED" -eq 1 ] || return 0
  printf '\n'
  printf '========================================\n'
  printf ' NanoQuant 初始管理员账号\n'
  printf ' 账号: admin\n'
  printf ' 密码: %s\n' "$ADMIN_PASSWORD"
  printf ' 首次登录后请立即修改密码。\n'
  printf '========================================\n'
  printf '\n'
}

on_error() {
  local code="$1"
  set +e
  log ERROR "安装失败（退出码 $code）"
  cleanup_password
  exit "$code"
}

on_interrupt() {
  set +e
  log ERROR "安装被中断"
  cleanup_password
  exit 130
}

trap 'on_error $?' ERR
trap on_interrupt INT TERM

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --systemd) SYSTEMD_MODE="yes"; shift ;;
      --no-systemd) SYSTEMD_MODE="no"; shift ;;
      --install-deps) INSTALL_DEPS=1; AUTO_DEPS=1; shift ;;
      --install-rust) INSTALL_RUST=1; AUTO_DEPS=1; shift ;;
      --install-node) INSTALL_NODE=1; AUTO_DEPS=1; shift ;;
      --no-auto-deps) AUTO_DEPS=0; shift ;;
      --offline) OFFLINE=1; shift ;;
      --skip-deps) SKIP_DEPS=1; shift ;;
      --skip-disk-check) SKIP_DISK_CHECK=1; shift ;;
      --password) [ -n "${2:-}" ] || die "--password 需要非空值"; ADMIN_PASSWORD="$2"; PASSWORD_FLAG=1; shift 2 ;;
      --password=*) [ -n "${1#*=}" ] || die "--password 需要非空值"; ADMIN_PASSWORD="${1#*=}"; PASSWORD_FLAG=1; shift ;;
      --password-stdin)
        IFS= read -r ADMIN_PASSWORD || true
        [ -n "$ADMIN_PASSWORD" ] || die "--password-stdin 未读到内容"
        PASSWORD_FLAG=1
        shift
        ;;
      --password-file)
        [ -n "${2:-}" ] && [ -f "$2" ] || die "--password-file 需要可读文件"
        ADMIN_PASSWORD="$(head -n 1 "$2")"
        [ -n "$ADMIN_PASSWORD" ] || die "--password-file 首行为空"
        chmod 600 "$2" 2>/dev/null || log WARN "无法将密码文件权限收紧为 0600: $2"
        PASSWORD_FLAG=1
        shift 2
        ;;
      --port) [ -n "${2:-}" ] || die "--port 需要非空值"; PORT="$2"; PORT_EXPLICIT=1; shift 2 ;;
      --port=*) [ -n "${1#*=}" ] || die "--port 需要非空值"; PORT="${1#*=}"; PORT_EXPLICIT=1; shift ;;
      --no-frontend) NO_FRONTEND=1; shift ;;
      --force-frontend) FORCE_FRONTEND=1; shift ;;
      --skip-build) SKIP_BUILD=1; shift ;;
      --no-start) NO_START=1; shift ;;
      --no-restart) NO_RESTART=1; shift ;;
      --check) CHECK_ONLY=1; shift ;;
      --user) [ -n "${2:-}" ] || die "--user 需要非空值"; RUN_USER="$2"; shift 2 ;;
      --user=*) [ -n "${1#*=}" ] || die "--user 需要非空值"; RUN_USER="${1#*=}"; shift ;;
      -h|--help) usage; exit 0 ;;
      *) usage >&2; die "未知参数: $1" ;;
    esac
  done
  if [ "$NO_FRONTEND" -eq 1 ] && [ "$FORCE_FRONTEND" -eq 1 ]; then
    die "--no-frontend 与 --force-frontend 不能同时使用"
  fi
  case "$PORT" in
    ''|*[!0-9]*) die "端口必须是数字: $PORT" ;;
  esac
  if [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
    die "端口超出范围: $PORT"
  fi
}

init_log() {
  if [ "$CHECK_ONLY" -eq 1 ]; then
    return
  fi
  mkdir -p "$LOG_DIR"
  chmod 700 "$LOG_DIR" 2>/dev/null || true
  find "$LOG_DIR" -maxdepth 1 -name 'install-*.log' -type f -mtime +14 -delete 2>/dev/null || true
  LOG_FILE="$LOG_DIR/install-$(date +%Y%m%d-%H%M%S).log"
  : >"$LOG_FILE"
  chmod 600 "$LOG_FILE" 2>/dev/null || true
  local commit distro arch
  commit="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  arch="$(uname -m 2>/dev/null || echo unknown)"
  distro="$(awk -F= '/^PRETTY_NAME=/{gsub(/"/, "", $2); print $2; exit}' /etc/os-release 2>/dev/null || echo unknown)"
  log INFO "安装日志: $LOG_FILE"
  log INFO "系统: $distro / $(uname -srm 2>/dev/null || echo unknown) / commit $commit"
  log INFO "仓库: $ROOT"
}

detect_pm() {
  if command -v apt-get >/dev/null 2>&1; then PM="apt"
  elif command -v dnf >/dev/null 2>&1; then PM="dnf"
  elif command -v yum >/dev/null 2>&1; then PM="yum"
  elif command -v apk >/dev/null 2>&1; then PM="apk"
  elif command -v zypper >/dev/null 2>&1; then PM="zypper"
  elif command -v pacman >/dev/null 2>&1; then PM="pacman"
  else PM=""
  fi
}

dependency_hint() {
  case "$PM" in
    apt) echo "apt-get update && apt-get install -y gcc libc6-dev make perl curl ca-certificates pkg-config git tar gzip xz-utils acl" ;;
    dnf) echo "dnf install -y gcc gcc-c++ glibc-devel make perl curl ca-certificates pkgconf-pkg-config git tar gzip xz acl" ;;
    yum) echo "yum install -y gcc gcc-c++ glibc-devel make perl curl ca-certificates pkgconfig git tar gzip xz acl" ;;
    apk) echo "apk add --no-cache gcc musl-dev make perl curl ca-certificates pkgconf git tar gzip xz acl" ;;
    zypper) echo "zypper --non-interactive install gcc gcc-c++ glibc-devel make perl curl ca-certificates pkg-config git tar gzip xz acl" ;;
    pacman) echo "pacman -Sy --needed --noconfirm gcc make perl curl ca-certificates pkgconf git tar gzip xz acl" ;;
    *) echo "请安装 gcc、make、perl、curl、git、tar、gzip、pkg-config" ;;
  esac
}

install_system_deps() {
  [ "$SYSTEM_DEPS_DONE" -eq 1 ] && return
  detect_pm
  [ -n "$PM" ] || die "未识别包管理器，无法自动安装系统依赖"
  log INFO "自动安装系统依赖（$PM）"
  case "$PM" in
    apt)
      run_root_logged env DEBIAN_FRONTEND=noninteractive apt-get update -qq
      run_root_logged env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        gcc libc6-dev make perl curl ca-certificates pkg-config git tar gzip xz-utils acl
      ;;
    dnf)
      run_root_logged dnf install -y \
        gcc gcc-c++ glibc-devel make perl curl ca-certificates pkgconf-pkg-config git tar gzip xz acl
      ;;
    yum)
      run_root_logged yum install -y \
        gcc gcc-c++ glibc-devel make perl curl ca-certificates pkgconfig git tar gzip xz acl
      ;;
    apk)
      run_root_logged apk add --no-cache \
        gcc musl-dev make perl curl ca-certificates pkgconf git tar gzip xz acl
      ;;
    zypper)
      run_root_logged zypper --non-interactive install \
        gcc gcc-c++ glibc-devel make perl curl ca-certificates pkg-config git tar gzip xz acl
      ;;
    pacman)
      run_root_logged pacman -Sy --needed --noconfirm \
        gcc make perl curl ca-certificates pkgconf git tar gzip xz acl
      ;;
  esac
  SYSTEM_DEPS_DONE=1
  hash -r 2>/dev/null || true
}

ensure_curl() {
  if ! command -v curl >/dev/null 2>&1; then
    if [ "$AUTO_DEPS" -eq 1 ] && [ "$OFFLINE" -eq 0 ]; then
      install_system_deps
    else
      die "缺少 curl；设置 NANOQUANT_AUTO_DEPS=1 或加 --install-deps，或手动安装"
    fi
  fi
  command -v curl >/dev/null 2>&1 || die "自动安装后仍缺少 curl"
}

ensure_native_deps() {
  local missing=()
  if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1; then
    missing+=("gcc")
  fi
  command -v make >/dev/null 2>&1 || missing+=("make")
  command -v perl >/dev/null 2>&1 || missing+=("perl")
  command -v pkg-config >/dev/null 2>&1 || missing+=("pkg-config")
  command -v setfacl >/dev/null 2>&1 || missing+=("setfacl")
  command -v git >/dev/null 2>&1 || missing+=("git")
  command -v tar >/dev/null 2>&1 || missing+=("tar")
  command -v gzip >/dev/null 2>&1 || missing+=("gzip")
  if [ "${#missing[@]}" -gt 0 ]; then
    if [ "$AUTO_DEPS" -eq 1 ] && [ "$OFFLINE" -eq 0 ]; then
      log INFO "缺少基础工具: ${missing[*]}；自动安装"
      install_system_deps
      missing=()
      if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1; then missing+=("gcc"); fi
      command -v make >/dev/null 2>&1 || missing+=("make")
      command -v perl >/dev/null 2>&1 || missing+=("perl")
      command -v pkg-config >/dev/null 2>&1 || missing+=("pkg-config")
      command -v setfacl >/dev/null 2>&1 || missing+=("setfacl")
      command -v git >/dev/null 2>&1 || missing+=("git")
      command -v tar >/dev/null 2>&1 || missing+=("tar")
      command -v gzip >/dev/null 2>&1 || missing+=("gzip")
    else
      die "缺少构建依赖: ${missing[*]}；请执行: $(dependency_hint)"
    fi
  fi
  if [ "${#missing[@]}" -gt 0 ]; then
    die "自动安装后仍缺少: ${missing[*]}；请执行: $(dependency_hint)"
  fi
  log OK "系统构建依赖就绪"
}

ensure_disk() {
  [ "$SKIP_DISK_CHECK" -eq 1 ] && return
  local avail
  avail="$(df -Pk "$ROOT" 2>/dev/null | awk 'NR==2 {print $4}' || true)"
  if [ -n "$avail" ] && [ "$avail" -lt $((MIN_FREE_MB * 1024)) ]; then
    die "磁盘空间不足：$((avail / 1024)) MB 可用，至少需要 ${MIN_FREE_MB} MB"
  fi
}

check_arch() {
  local arch
  arch="$(uname -m)"
  case "$arch" in
    x86_64|aarch64|arm64) log OK "架构: $arch" ;;
    *) log WARN "未验证的架构: $arch；仅测试 x86_64/aarch64" ;;
  esac
}

resolve_port() {
  if [ "$PORT_EXPLICIT" -eq 0 ] && [ -f "$ROOT/data/.port" ]; then
    local existing
    existing="$(tr -dc '0-9' <"$ROOT/data/.port" || true)"
    if [ -n "$existing" ] && [ "$existing" -ge 1 ] && [ "$existing" -le 65535 ]; then
      PORT="$existing"
    else
      log WARN "data/.port 内容无效，使用 $DEFAULT_PORT"
    fi
  fi
}

check_port() {
  local pattern="[:.]$PORT$"
  if command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | awk '{print $4}' | grep -qE "$pattern"; then
    log WARN "端口 $PORT 已被监听；升级安装可忽略"
  elif command -v netstat >/dev/null 2>&1 && netstat -ltn 2>/dev/null | awk '{print $4}' | grep -qE "$pattern"; then
    log WARN "端口 $PORT 已被监听；升级安装可忽略"
  fi
}

read_rust_min() {
  local from_cargo
  from_cargo="$(awk -F'"' '/^rust-version[[:space:]]*=/{print $2; exit}' "$ROOT/rust/nanoquant/Cargo.toml" 2>/dev/null || true)"
  RUST_MIN="${NANOQUANT_MIN_RUST:-${from_cargo:-1.88}}"
}

ensure_rust() {
  read_rust_min
  if ! command -v cargo >/dev/null 2>&1 && [ -n "${SUDO_USER:-}" ] && [ "$(id -u)" -eq 0 ]; then
    local sudo_home=""
    sudo_home="$(getent passwd "$SUDO_USER" 2>/dev/null | cut -d: -f6 || true)"
    if [ -n "$sudo_home" ] && [ -x "$sudo_home/.cargo/bin/cargo" ]; then
      export PATH="$sudo_home/.cargo/bin:$PATH"
      log INFO "复用 $SUDO_USER 的 Rust 工具链"
    fi
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    if [ "$AUTO_DEPS" -eq 1 ] && [ "$OFFLINE" -eq 0 ]; then
      ensure_curl
      log INFO "通过 rustup 安装 minimal Rust 工具链"
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
      if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
      fi
      export PATH="$HOME/.cargo/bin:$PATH"
      hash -r 2>/dev/null || true
    else
      die "缺少 cargo/rustc；请安装 Rust $RUST_MIN+，或联网运行安装脚本"
    fi
  fi
  command -v rustc >/dev/null 2>&1 || {
    if [ "$AUTO_DEPS" -eq 1 ] && [ "$OFFLINE" -eq 0 ]; then
      ensure_curl
      if command -v rustup >/dev/null 2>&1; then
        rustup update stable
        rustup default stable
      else
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
      fi
      if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
      export PATH="$HOME/.cargo/bin:$PATH"
      hash -r 2>/dev/null || true
    else
      die "缺少 rustc；请安装完整 Rust 工具链"
    fi
  }

  local version major minor req_major req_minor
  version="$(rustc --version | awk '{print $2}')"
  major="${version%%.*}"
  minor="${version#*.}"
  minor="${minor%%.*}"
  IFS=. read -r req_major req_minor _ <<<"$RUST_MIN"
  if [ "$major" -lt "$req_major" ] || { [ "$major" -eq "$req_major" ] && [ "$minor" -lt "$req_minor" ]; }; then
    if [ "$AUTO_DEPS" -eq 1 ] && [ "$OFFLINE" -eq 0 ]; then
      ensure_curl
      log INFO "Rust $version 低于 $RUST_MIN，自动升级 stable"
      if command -v rustup >/dev/null 2>&1; then
        rustup update stable
        rustup default stable
      else
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
      fi
      if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
      export PATH="$HOME/.cargo/bin:$PATH"
      hash -r 2>/dev/null || true
      version="$(rustc --version | awk '{print $2}')"
      major="${version%%.*}"
      minor="${version#*.}"
      minor="${minor%%.*}"
    fi
    if [ "$major" -lt "$req_major" ] || { [ "$major" -eq "$req_major" ] && [ "$minor" -lt "$req_minor" ]; }; then
      die "Rust $version 过低，需要 $RUST_MIN+"
    fi
  fi
  log OK "Rust $version（要求 $RUST_MIN+）"
}

install_node() {
  [ "$(id -u)" -eq 0 ] || [ -n "$SUDO" ] || die "自动安装 Node.js 需要 root 或 sudo"
  [ "$OFFLINE" -eq 0 ] || die "--offline 模式无法安装 Node.js"
  detect_pm
  ensure_curl
  log INFO "自动安装 Node.js 20+（$PM）"
  case "$PM" in
    apt)
      if [ "$(id -u)" -eq 0 ]; then
        curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
      else
        curl -fsSL https://deb.nodesource.com/setup_20.x | "$SUDO" bash -
      fi
      run_root_logged env DEBIAN_FRONTEND=noninteractive apt-get install -y nodejs
      ;;
    dnf|yum)
      if [ "$(id -u)" -eq 0 ]; then
        curl -fsSL https://rpm.nodesource.com/setup_20.x | bash -
      else
        curl -fsSL https://rpm.nodesource.com/setup_20.x | "$SUDO" bash -
      fi
      run_root_logged "$PM" install -y nodejs
      ;;
    apk) run_root_logged apk add --no-cache nodejs npm ;;
    pacman) run_root_logged pacman -Sy --needed --noconfirm nodejs npm ;;
    zypper) run_root_logged zypper --non-interactive install nodejs20 npm20 || run_root_logged zypper --non-interactive install nodejs npm ;;
    *) die "未识别的包管理器，无法自动安装 Node.js 20+" ;;
  esac
  hash -r 2>/dev/null || true
}

ensure_node() {
  if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
    if [ "$AUTO_DEPS" -eq 1 ] && [ "$OFFLINE" -eq 0 ]; then
      install_node
    else
      die "缺少 Node.js/npm；请安装 Node.js $MIN_NODE_MAJOR+，或联网运行安装脚本"
    fi
  fi
  local major
  major="$(node -p 'process.versions.node.split(".")[0]')"
  if [ "$major" -lt "$MIN_NODE_MAJOR" ]; then
    if [ "$AUTO_DEPS" -eq 1 ] && [ "$OFFLINE" -eq 0 ]; then
      log INFO "Node.js $major 低于 $MIN_NODE_MAJOR，自动安装 Node.js $MIN_NODE_MAJOR"
      install_node
      major="$(node -p 'process.versions.node.split(".")[0]')"
    fi
    if [ "$major" -lt "$MIN_NODE_MAJOR" ]; then
      die "Node.js $major 过低，需要 $MIN_NODE_MAJOR+；建议用 nvm 安装"
    fi
  fi
  [ -f "$ROOT/frontend/package-lock.json" ] || die "缺少 frontend/package-lock.json"
  log OK "Node.js $(node --version), npm $(npm --version)"
}

frontend_stale() {
  [ -f "$FRONTEND_INDEX" ] || return 0
  if find "$ROOT/frontend/src" "$ROOT/frontend/index.html" "$ROOT/frontend/package.json" \
    "$ROOT/frontend/package-lock.json" "$ROOT/frontend/vite.config.ts" "$ROOT/frontend/tsconfig.json" \
    -newer "$FRONTEND_INDEX" -print -quit 2>/dev/null | grep -q .; then
    return 0
  fi
  return 1
}

frontend_needs_build() {
  [ "$NO_FRONTEND" -eq 1 ] && return 1
  [ -f "$FRONTEND_INDEX" ] || return 0
  [ "$FORCE_FRONTEND" -eq 1 ] && return 0
  if frontend_stale; then
    log WARN "前端源码比 dist 新，自动重建；可用 --no-frontend 跳过"
    FORCE_FRONTEND=1
    return 0
  fi
  return 1
}

backup_binary() {
  if [ -x "$BIN" ]; then
    mkdir -p "$ROOT/data/backups"
    BACKUP_BIN="$ROOT/data/backups/nanoquant-$(date +%Y%m%d%H%M%S)"
    cp -a "$BIN" "$BACKUP_BIN"
    chmod 700 "$BACKUP_BIN" 2>/dev/null || true
    log INFO "已备份旧二进制: $BACKUP_BIN"
  fi
}

build_backend() {
  if [ "$SKIP_BUILD" -eq 1 ]; then
    [ -x "$BIN" ] || die "--skip-build 但未找到可执行二进制: $BIN"
    log OK "复用已编译二进制"
    return
  fi
  backup_binary
  local args=(build --release --locked --manifest-path "$ROOT/rust/nanoquant/Cargo.toml")
  if [ "$OFFLINE" -eq 1 ]; then args+=(--offline); fi
  log INFO "构建 Rust 后端（release, locked）"
  run_logged cargo "${args[@]}"
  [ -x "$BIN" ] || die "构建后未找到二进制: $BIN"
  chmod 755 "$BIN"
  log OK "Rust 二进制已构建"
}

build_frontend() {
  if [ "$NO_FRONTEND" -eq 1 ]; then
    [ -f "$FRONTEND_INDEX" ] || die "--no-frontend 但 frontend/dist/index.html 不存在"
    if frontend_stale; then
      log WARN "--no-frontend：dist 可能落后于前端源码，继续使用现有 dist"
    else
      log OK "复用已有前端 dist"
    fi
    return
  fi
  if ! frontend_needs_build; then
    log OK "复用已有前端 dist"
    return
  fi
  ensure_node
  local args=(--prefix "$ROOT/frontend" ci --no-audit --no-fund)
  if [ "$OFFLINE" -eq 1 ]; then args+=(--offline); fi
  log INFO "安装前端依赖（npm ci）"
  run_logged npm "${args[@]}"
  log INFO "构建前端"
  run_logged npm --prefix "$ROOT/frontend" run build
  [ -f "$FRONTEND_INDEX" ] || die "前端构建后未找到 frontend/dist/index.html"
  chmod -R a+rX "$FRONTEND_DIST" 2>/dev/null || true
  log OK "前端 dist 已构建"
}

create_service_user() {
  if [ "$SYSTEMD" -eq 0 ]; then
    if [ -n "$RUN_USER" ]; then
      die "--user 仅支持 --systemd 模式"
    fi
    return
  fi
  if [ -z "$RUN_USER" ]; then
    RUN_USER="nanoquant"
    if ! id -u "$RUN_USER" >/dev/null 2>&1; then
      if command -v useradd >/dev/null 2>&1; then
        useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$RUN_USER" \
          || die "创建系统用户 $RUN_USER 失败"
      elif command -v adduser >/dev/null 2>&1; then
        adduser -S -D -H -h /nonexistent -s /sbin/nologin "$RUN_USER" \
          || die "创建系统用户 $RUN_USER 失败"
      else
        die "无法创建系统用户 $RUN_USER；请手动创建或使用 --user"
      fi
      CREATED_USER=1
      log OK "已创建系统用户 $RUN_USER"
    fi
  fi
  id -u "$RUN_USER" >/dev/null 2>&1 || die "系统用户不存在: $RUN_USER"
  RUN_GROUP="$(id -gn "$RUN_USER")"
}

grant_service_access() {
  [ "$SYSTEMD" -eq 1 ] || return 0
  [ -n "$RUN_USER" ] || return 0
  [ "$RUN_USER" = "root" ] && return 0
  command -v runuser >/dev/null 2>&1 || {
    log WARN "缺少 runuser，跳过服务用户路径访问预检"
    return 0
  }
  if runuser -u "$RUN_USER" -- test -x "$ROOT" >/dev/null 2>&1 \
    && runuser -u "$RUN_USER" -- test -r "$BIN" >/dev/null 2>&1; then
    return 0
  fi
  log WARN "服务用户 $RUN_USER 无法访问安装目录，自动授予路径穿越权限"
  local path start
  for start in "$ROOT" "$(dirname "$BIN")" "$(dirname "$FRONTEND_DIST")"; do
    path="$start"
    while [ "$path" != "/" ]; do
      if ! runuser -u "$RUN_USER" -- test -x "$path" >/dev/null 2>&1; then
        if command -v setfacl >/dev/null 2>&1 \
          && setfacl -m "u:$RUN_USER:x" "$path" 2>/dev/null; then
          log INFO "ACL 授权 $RUN_USER 穿越 $path"
        else
          chmod o+x "$path" 2>/dev/null || die "无法授予 $RUN_USER 穿越 $path"
          log WARN "已回退 chmod o+x：$path"
        fi
      fi
      path="$(dirname "$path")"
    done
  done
  if ! runuser -u "$RUN_USER" -- test -r "$BIN" >/dev/null 2>&1; then
    if command -v setfacl >/dev/null 2>&1 \
      && setfacl -m "u:$RUN_USER:rX" "$BIN" 2>/dev/null; then
      log INFO "ACL 授权 $RUN_USER 读取 $BIN"
    else
      chmod o+rx "$BIN" 2>/dev/null || die "无法授予 $RUN_USER 读取 $BIN"
      log WARN "已回退 chmod o+rx：$BIN"
    fi
  fi
  runuser -u "$RUN_USER" -- test -x "$ROOT" >/dev/null 2>&1 \
    || die "服务用户 $RUN_USER 仍无法进入 $ROOT"
  runuser -u "$RUN_USER" -- test -r "$BIN" >/dev/null 2>&1 \
    || die "服务用户 $RUN_USER 仍无法读取 $BIN"
  if [ -f "$FRONTEND_INDEX" ]; then
    runuser -u "$RUN_USER" -- test -r "$FRONTEND_INDEX" >/dev/null 2>&1 \
      || die "服务用户 $RUN_USER 仍无法读取 $FRONTEND_INDEX"
  fi
  log OK "服务用户 $RUN_USER 已可访问安装目录"
}

prepare_dirs() {
  mkdir -p "$ROOT/data" "$ROOT/logs" "$ROOT/data/backups"
  if ! chmod 700 "$ROOT/data" "$ROOT/logs" "$ROOT/data/backups" 2>/dev/null; then
    die "无法将 data/logs 权限收紧为 0700"
  fi
  local port_file="$ROOT/data/.port"
  if [ "$(cat "$port_file" 2>/dev/null || true)" != "$PORT" ]; then
    printf '%s\n' "$PORT" >"$port_file"
  fi
  chmod 600 "$port_file" 2>/dev/null || true
  for file in "$ROOT/data/.master_key" "$ROOT/data/nanoquant.db"; do
    if [ -f "$file" ]; then
      chmod 600 "$file" 2>/dev/null || log WARN "无法收紧权限: $file"
    fi
  done
  if [ -n "$RUN_USER" ]; then
    chown -R "$RUN_USER:$RUN_GROUP" "$ROOT/data" "$ROOT/logs" || die "chown 数据目录失败"
    if [ -x "$BIN" ]; then
      chown "$RUN_USER:$RUN_GROUP" "$BIN"
      chmod 755 "$BIN"
    fi
    if [ -d "$FRONTEND_DIST" ]; then
      chown -R "$RUN_USER:$RUN_GROUP" "$FRONTEND_DIST"
    fi
    if command -v runuser >/dev/null 2>&1; then
      runuser -u "$RUN_USER" -- test -r "$BIN" >/dev/null 2>&1 \
        || die "服务用户 $RUN_USER 仍无法读取 $BIN；请检查 ACL 或目录权限"
      runuser -u "$RUN_USER" -- test -x "$ROOT" >/dev/null 2>&1 \
        || die "服务用户 $RUN_USER 仍无法进入 $ROOT；请检查 ACL 或目录权限"
    fi
  fi
  log OK "数据目录就绪（端口 $PORT）"
}

healthcheck() {
  local url="http://127.0.0.1:$PORT/api/v1/health"
  local i
  if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    log ERROR "缺少 curl/wget，无法进行健康检查"
    return 1
  fi
  for i in $(seq 1 30); do
    if command -v curl >/dev/null 2>&1 && curl -fsS --max-time 2 "$url" >/dev/null 2>&1; then
      return 0
    fi
    if command -v wget >/dev/null 2>&1 && wget -qO- --timeout=2 "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

align_exec_core_user() {
  local unit="/etc/systemd/system/nanoquant-exec-core.service"
  [ -f "$unit" ] || return 0
  local current
  current="$(awk -F= '/^User=/{gsub(/[[:space:]]/, "", $2); print $2; exit}' "$unit")"
  if [ -n "$current" ] && [ "$current" != "$RUN_USER" ] && [ "$current" != "root" ]; then
    log WARN "执行内核运行用户 $current 与主服务 $RUN_USER 不一致；请用 --user $RUN_USER 重新运行 setup_core.sh"
    return 0
  fi
  if [ -z "$current" ] || [ "$current" = "root" ]; then
    cp -a "$unit" "$unit.bak.$(date +%Y%m%d%H%M%S)"
    if grep -q '^User=' "$unit"; then
      sed -i "s/^User=.*/User=$RUN_USER/" "$unit"
    else
      sed -i "/^\\[Service\\]/a User=$RUN_USER" "$unit"
    fi
    if grep -q '^Group=' "$unit"; then
      sed -i "s/^Group=.*/Group=$RUN_GROUP/" "$unit"
    else
      sed -i "/^User=$RUN_USER$/a Group=$RUN_GROUP" "$unit"
    fi
    systemctl daemon-reload
    systemctl restart nanoquant-exec-core 2>/dev/null || log WARN "nanoquant-exec-core 重启失败；请检查 journalctl -u nanoquant-exec-core"
    log OK "已将执行内核 unit 运行用户对齐为 $RUN_USER"
  fi
}

rollback_binary() {
  if [ -n "$BACKUP_BIN" ] && [ -f "$BACKUP_BIN" ]; then
    install -m 755 "$BACKUP_BIN" "$BIN" || return 1
    log WARN "已恢复旧二进制"
    return 0
  fi
  return 1
}

systemd_unit() {
  printf '[Unit]\n'
  printf 'Description=NanoQuant (Rust trading engine)\n'
  printf 'After=network-online.target nanoquant-exec-core.service\n'
  printf 'Wants=network-online.target\n\n'
  printf '[Service]\n'
  printf 'Type=simple\n'
  printf 'User=%s\nGroup=%s\n' "$RUN_USER" "$RUN_GROUP"
  printf 'WorkingDirectory=%s\n' "$ROOT"
  printf 'Environment=NANOQUANT_HOME=%s\n' "$ROOT"
  printf 'Environment=NANOQUANT_PORT=%s\n' "$PORT"
  printf 'EnvironmentFile=-%s\n' "$ENV_FILE"
  printf 'ExecStart=%s\n' "$BIN"
  printf 'Restart=always\n'
  printf 'RestartSec=5\n'
  printf 'KillSignal=SIGINT\n'
  printf 'TimeoutStopSec=30\n'
  printf 'LimitNOFILE=65535\n'
  printf 'UMask=0077\n'
  printf 'NoNewPrivileges=true\n'
  printf 'PrivateTmp=true\n'
  printf 'ProtectSystem=strict\n'
  printf 'ProtectKernelTunables=true\n'
  printf 'ProtectKernelModules=true\n'
  printf 'ProtectControlGroups=true\n'
  printf 'RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6\n'
  printf 'RestrictNamespaces=true\n'
  printf 'LockPersonality=true\n'
  printf 'RestrictRealtime=true\n'
  printf 'RestrictSUIDSGID=true\n'
  printf 'SystemCallArchitectures=native\n'
  printf 'ReadWritePaths=%s/data %s/logs\n' "$ROOT" "$ROOT"
  printf '\n[Install]\nWantedBy=multi-user.target\n'
}

install_systemd() {
  local unit="/etc/systemd/system/nanoquant.service"
  local env_dir="/etc/nanoquant"
  ENV_FILE="$env_dir/nanoquant.env"
  [ "$(id -u)" -eq 0 ] || die "--systemd 需要 root 权限，请使用 sudo"
  command -v systemctl >/dev/null 2>&1 || die "缺少 systemctl"
  [ -d /run/systemd/system ] || die "当前系统未运行 systemd"

  align_exec_core_user
  if [ -n "$ADMIN_PASSWORD" ]; then
    case "$ADMIN_PASSWORD" in
      *$'\n'*) die "管理员密码不能包含换行" ;;
    esac
    mkdir -p "$env_dir"
    chmod 700 "$env_dir"
    ( umask 077; printf 'NANOQUANT_ADMIN_PASSWORD=%s\n' "$ADMIN_PASSWORD" >"$ENV_FILE" )
    chmod 600 "$ENV_FILE"
    ENV_FILE_CREATED=1
    log OK "管理员初始密码写入 $ENV_FILE（0600）"
  elif [ -f "$ENV_FILE" ]; then
    chmod 600 "$ENV_FILE" 2>/dev/null || log WARN "无法收紧 $ENV_FILE 权限"
  fi
  if [ -f "$unit" ]; then
    BACKUP_UNIT="$unit.bak.$(date +%Y%m%d%H%M%S)"
    cp -a "$unit" "$BACKUP_UNIT"
    log INFO "已备份旧 unit: $BACKUP_UNIT"
  fi
  systemd_unit >"$unit"
  chmod 644 "$unit"
  log INFO "systemd unit: $unit（User=$RUN_USER）"
  systemctl daemon-reload
  if [ "$NO_START" -eq 1 ]; then
    log OK "unit 已写入；按要求未启动或启用。启动: systemctl enable --now nanoquant"
    return
  fi
  if [ "$NO_RESTART" -eq 1 ]; then
    log OK "unit 已写入；按要求未重启。应用新版本: systemctl restart nanoquant"
    return
  fi
  systemctl enable nanoquant >/dev/null
  if ! systemctl restart nanoquant; then
    systemctl status nanoquant --no-pager -l 2>&1 | tee -a "${LOG_FILE:-/dev/null}" >&2 || true
    journalctl -u nanoquant -n 80 --no-pager 2>&1 | tee -a "${LOG_FILE:-/dev/null}" >&2 || true
    if [ -n "$BACKUP_UNIT" ] && [ -f "$BACKUP_UNIT" ]; then cp -a "$BACKUP_UNIT" "$unit"; fi
    rollback_binary || true
    systemctl daemon-reload
    systemctl restart nanoquant 2>/dev/null || systemctl stop nanoquant 2>/dev/null || true
    die "systemd 启动失败"
  fi
  if ! healthcheck; then
    systemctl status nanoquant --no-pager -l 2>&1 | tee -a "${LOG_FILE:-/dev/null}" >&2 || true
    journalctl -u nanoquant -n 80 --no-pager 2>&1 | tee -a "${LOG_FILE:-/dev/null}" >&2 || true
    if [ -n "$BACKUP_UNIT" ] && [ -f "$BACKUP_UNIT" ]; then cp -a "$BACKUP_UNIT" "$unit"; fi
    rollback_binary || true
    systemctl daemon-reload
    if systemctl restart nanoquant 2>/dev/null && healthcheck; then
      log WARN "新版本失败，已回滚到旧版本"
    else
      systemctl stop nanoquant 2>/dev/null || true
      log ERROR "回滚后仍未通过健康检查"
    fi
    die "健康检查失败"
  fi
  if [ -n "$ADMIN_PASSWORD" ] && [ -f "$ENV_FILE" ]; then
    rm -f "$ENV_FILE"
    ENV_FILE_CREATED=0
    ENV_FILE=""
    log OK "已移除一次性管理员密码文件"
  fi
  log OK "systemd 服务已启动并通过健康检查: http://127.0.0.1:$PORT"
}

install_nohup() {
  [ -f "$ROOT/scripts/start.sh" ] || die "缺少 scripts/start.sh"
  [ -f "$ROOT/scripts/stop.sh" ] || die "缺少 scripts/stop.sh"
  if [ "$(id -u)" -eq 0 ]; then
    log WARN "nohup 模式以 root 运行；建议在 systemd 环境使用 --systemd"
  fi
  if [ "$NO_START" -eq 1 ]; then
    if [ -n "$ADMIN_PASSWORD" ]; then
      log WARN "启动时请设置 NANOQUANT_ADMIN_PASSWORD 后执行 bash scripts/start.sh --daemon --no-systemd"
    else
      log OK "安装完成；按要求未启动。启动: bash scripts/start.sh --daemon --no-systemd"
    fi
    return
  fi
  local pid_file="$ROOT/data/nanoquant.pid"
  if [ "$NO_RESTART" -eq 1 ] && [ -f "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
    log OK "服务正在运行；按要求未重启，新二进制未生效"
    return
  fi
  bash "$ROOT/scripts/stop.sh" --no-systemd >/dev/null 2>&1 || true
  if [ -n "$ADMIN_PASSWORD" ]; then
    export NANOQUANT_ADMIN_PASSWORD="$ADMIN_PASSWORD"
  fi
  if ! bash "$ROOT/scripts/start.sh" --daemon --no-systemd; then
    unset NANOQUANT_ADMIN_PASSWORD || true
    rollback_binary || true
    die "nohup 启动失败"
  fi
  unset NANOQUANT_ADMIN_PASSWORD || true
  if ! healthcheck; then
    bash "$ROOT/scripts/stop.sh" --no-systemd >/dev/null 2>&1 || true
    if rollback_binary; then
      if bash "$ROOT/scripts/start.sh" --daemon --no-systemd && healthcheck; then
        log WARN "新版本失败，已回滚到旧版本"
      else
        bash "$ROOT/scripts/stop.sh" --no-systemd >/dev/null 2>&1 || true
        log ERROR "回滚后仍未通过健康检查"
      fi
    fi
    die "健康检查失败"
  fi
  log OK "nohup 服务已启动并通过健康检查: http://127.0.0.1:$PORT"
}

check_only() {
  ensure_disk
  check_arch
  resolve_port
  check_port
  if [ "$SKIP_DEPS" -eq 0 ]; then
    ensure_rust
    ensure_native_deps
    if frontend_needs_build; then
      ensure_node
    fi
  fi
  if [ "$SYSTEMD_MODE" = "yes" ]; then
    [ "$(id -u)" -eq 0 ] || die "--systemd 需要 root 权限"
    command -v systemctl >/dev/null 2>&1 || die "缺少 systemctl"
    [ -d /run/systemd/system ] || die "当前系统未运行 systemd"
    if [ -z "$RUN_USER" ]; then RUN_USER="nanoquant"; fi
    if ! id -u "$RUN_USER" >/dev/null 2>&1; then
      log WARN "systemd 用户 $RUN_USER 不存在；安装时将自动创建（仅默认 nanoquant）"
    fi
  fi
  if [ "$SKIP_BUILD" -eq 1 ] && [ ! -x "$BIN" ]; then die "--skip-build 但二进制不存在"; fi
  if [ "$NO_FRONTEND" -eq 1 ] && [ ! -f "$FRONTEND_INDEX" ]; then die "--no-frontend 但 dist 不存在"; fi
  if [ -x "$BIN" ]; then log OK "二进制已存在"; else log WARN "二进制尚需构建"; fi
  if [ -f "$FRONTEND_INDEX" ]; then
    if [ "$NO_FRONTEND" -eq 0 ] && frontend_stale; then log WARN "前端 dist 落后于源码，安装时将重建"; else log OK "前端 dist 已存在"; fi
  else
    log WARN "前端 dist 尚需构建"
  fi
  log OK "环境检查通过；未修改任何文件"
  exit 0
}

main() {
  parse_args "$@"
  if [ "$CHECK_ONLY" -eq 1 ] || [ "$OFFLINE" -eq 1 ]; then
    AUTO_DEPS=0
  fi
  init_log
  detect_pm
  SUDO=""
  if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
  fi
  if [ "$SYSTEMD_MODE" = "yes" ]; then
    SYSTEMD=1
    [ "$(id -u)" -eq 0 ] || die "--systemd 需要 root 权限"
    command -v systemctl >/dev/null 2>&1 || die "缺少 systemctl"
    [ -d /run/systemd/system ] || die "当前系统未运行 systemd"
  elif [ "$SYSTEMD_MODE" = "no" ]; then
    SYSTEMD=0
  else
    if [ "$(id -u)" -eq 0 ] && [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
      SYSTEMD=1
    else
      SYSTEMD=0
    fi
  fi
  if [ "$SYSTEMD" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then
    die "systemd 模式需要 root；请使用 sudo 或加 --no-systemd"
  fi
  if [ "$SYSTEMD" -eq 0 ] && [ -n "$RUN_USER" ]; then
    die "--user 仅支持 systemd 模式"
  fi
  if [ "$CHECK_ONLY" -eq 1 ]; then
    check_only
  fi

  if [ "$SKIP_DEPS" -eq 0 ]; then
    ensure_native_deps
    ensure_curl
    ensure_rust
  fi
  ensure_disk
  check_arch
  resolve_port
  check_port
  if frontend_needs_build; then
    ensure_node
  fi
  build_backend
  build_frontend
  if [ "$SYSTEMD" -eq 1 ]; then
    create_service_user
    grant_service_access
  fi
  prepare_dirs
  if [ -z "$ADMIN_PASSWORD" ]; then
    case "$(admin_password_state)" in
      present)
        log OK "检测到已有管理员密码；跳过自动生成"
        ;;
      missing)
        ADMIN_PASSWORD="$(generate_admin_password)"
        PASSWORD_GENERATED=1
        log INFO "未提供管理员密码，已自动生成；安装完成后会打印一次"
        ;;
      *)
        log WARN "已有数据目录但无法确认管理员状态；本次不自动生成密码"
        ;;
    esac
  fi
  if [ "$SYSTEMD" -eq 1 ]; then
    install_systemd
  else
    install_nohup
  fi
  log OK "安装完成"
  if [ -n "$BACKUP_BIN" ] && [ -f "$BACKUP_BIN" ]; then log INFO "回滚二进制: $BACKUP_BIN"; fi
  log INFO "端口: $PORT；模式: $([ "$SYSTEMD" -eq 1 ] && echo systemd || echo nohup)"
  log INFO "日志: ${LOG_FILE:-未写入（--check）}"
  print_generated_password
}

main "$@"