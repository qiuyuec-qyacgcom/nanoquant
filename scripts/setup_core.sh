#!/usr/bin/env bash
# 部署 Rust 执行内核 exec-core（默认组件）
#
# exec-core 负责签名校验与七段延迟指标聚合。systemd 模式通过 UDS 与主服务通信；
# nohup/容器模式通过 EXEC_TCP 通信。
#
# 用法（仓库根目录）：
#   sudo bash scripts/setup_core.sh --from-settings
#   sudo bash scripts/setup_core.sh --secret '<交易所签名密钥>'
#   bash scripts/setup_core.sh --no-systemd --from-settings
#
# 常用参数：
#   --secret <s>       与交易所 API secret 相同的密钥（Ed25519 私钥 base64 PKCS#8 或 HMAC secret）
#   --from-settings    从主服务数据库读取当前交易环境的交易所 Secret
#   --skip-build       复用已编译的 exec-core 二进制
#   --home <dir>       主服务仓库目录，默认当前仓库根目录
#   --user <name>      systemd 运行用户；默认复用 nanoquant 用户，否则 root
#   --systemd          强制 systemd 模式（默认）
#   --no-systemd       安装到仓库目录并使用 EXEC_TCP，不注册 systemd
#   --no-start         只安装/启用，不启动或重启（升级时保持旧进程）
#   -h, --help         显示帮助
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APP_HOME="$REPO_ROOT"
SECRET=""
FROM_SETTINGS=0
SKIP_BUILD=0
SKIP_DEPS=0
OFFLINE=0
RUN_USER="${NANOQUANT_USER:-}"
RUN_GROUP=""
MODE="systemd"
NO_START=0
RUST_MIN="${NANOQUANT_MIN_RUST:-1.88}"
UNIT_NAME="nanoquant-exec-core.service"
UNIT_PATH="/etc/systemd/system/$UNIT_NAME"

info() { printf '\033[1;34m[exec-core]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[exec-core]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[exec-core]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[exec-core]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
用法: bash scripts/setup_core.sh [选项]

执行内核 exec-core 是 NanoQuant 的默认交易组件；没有它，主服务 HTTP 可用，
但交易引擎不会启动。

选项:
  --secret <secret>   与交易所 API secret 相同的签名密钥
  --secret-stdin      从标准输入读取签名密钥（避免出现在命令历史/进程列表）
  --from-settings     从主服务数据库读取当前交易环境的 Secret
  --skip-build        复用已编译的 rust/exec-core/target/release/exec-core
  --skip-deps         跳过构建依赖检查
  --offline           cargo 使用离线模式；缺少工具时不联网安装
  --home <目录>       主服务仓库目录，默认脚本所在仓库
  --user <用户>       systemd 运行用户；默认复用 nanoquant，否则 root
  --systemd           强制 systemd 模式（默认）
  --no-systemd        安装到仓库目录，使用 EXEC_TCP=127.0.0.1:9101
  --no-start          只安装/启用，不启动或重启服务
  -h, --help          显示帮助

模式:
  systemd  安装 $APP_HOME/exec-core，写 /etc/nanoquant/exec.env，
           生成并启用 nanoquant-exec-core.service
  nohup    安装 $APP_HOME/exec-core，写 $APP_HOME/data/exec.env，
           由 scripts/start.sh --no-systemd 启动

没有 Secret 时只安装二进制/unit，不会启动；配置 API 凭证后重新运行：
  sudo bash scripts/setup_core.sh --from-settings
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --secret) SECRET="${2:-}"; [ -n "$SECRET" ] || die "--secret 需要非空值"; shift 2 ;;
    --secret=*) SECRET="${1#*=}"; [ -n "$SECRET" ] || die "--secret 需要非空值"; shift ;;
    --secret-stdin)
      [ -z "$SECRET" ] || die "--secret 与 --secret-stdin 不能同时使用"
      IFS= read -r SECRET || true
      [ -n "$SECRET" ] || die "--secret-stdin 未读到内容"
      shift
      ;;
    --from-settings) FROM_SETTINGS=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --skip-deps) SKIP_DEPS=1; shift ;;
    --offline) OFFLINE=1; shift ;;
    --home) APP_HOME="${2:-}"; [ -n "$APP_HOME" ] || die "--home 需要非空值"; shift 2 ;;
    --home=*) APP_HOME="${1#*=}"; [ -n "$APP_HOME" ] || die "--home 需要非空值"; shift ;;
    --user) RUN_USER="${2:-}"; [ -n "$RUN_USER" ] || die "--user 需要非空值"; shift 2 ;;
    --user=*) RUN_USER="${1#*=}"; [ -n "$RUN_USER" ] || die "--user 需要非空值"; shift ;;
    --systemd) MODE="systemd"; shift ;;
    --no-systemd|--nohup) MODE="nohup"; shift ;;
    --no-start) NO_START=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "未知参数: $1" ;;
  esac
done

case "$SECRET" in
  *$'\n'*) die "Secret 不能包含换行" ;;
esac

if [ "$MODE" = "systemd" ]; then
  [ "$(id -u)" -eq 0 ] || die "systemd 模式需要 root：sudo bash scripts/setup_core.sh ..."
  command -v systemctl >/dev/null 2>&1 || die "缺少 systemctl，无法注册 exec-core 服务"
else
  mkdir -p "$APP_HOME/data" "$APP_HOME/logs"
  [ -w "$APP_HOME" ] || die "目录不可写: $APP_HOME"
fi

run_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    die "自动安装系统依赖需要 root 或 sudo"
  fi
}

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

ensure_core_deps() {
  local pm missing=()
  pm="$(detect_pm)"
  command -v curl >/dev/null 2>&1 || missing+=("curl")
  if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1; then missing+=("gcc"); fi
  command -v make >/dev/null 2>&1 || missing+=("make")
  command -v perl >/dev/null 2>&1 || missing+=("perl")
  command -v pkg-config >/dev/null 2>&1 || missing+=("pkg-config")
  if [ "$MODE" = "systemd" ]; then
    command -v setfacl >/dev/null 2>&1 || missing+=("setfacl")
  fi
  [ "${#missing[@]}" -gt 0 ] || return 0
  [ "$OFFLINE" -eq 0 ] || die "缺少 ${missing[*]}，且当前为 --offline，无法自动安装"
  [ -n "$pm" ] || die "缺少 ${missing[*]}，且未识别包管理器；无法自动安装 exec-core 构建依赖"
  info "自动安装 exec-core 构建依赖（$pm）：${missing[*]}"
  case "$pm" in
    apt)
      run_root env DEBIAN_FRONTEND=noninteractive apt-get update -qq
      run_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        gcc libc6-dev make perl curl ca-certificates pkg-config acl
      ;;
    dnf) run_root dnf install -y gcc gcc-c++ glibc-devel make perl curl ca-certificates pkgconf-pkg-config acl ;;
    yum) run_root yum install -y gcc gcc-c++ glibc-devel make perl curl ca-certificates pkgconfig acl ;;
    apk) run_root apk add --no-cache gcc musl-dev make perl curl ca-certificates pkgconf acl ;;
    zypper) run_root zypper --non-interactive install gcc gcc-c++ glibc-devel make perl curl ca-certificates pkg-config acl ;;
    pacman) run_root pacman -Sy --needed --noconfirm gcc make perl curl ca-certificates pkgconf acl ;;
  esac
  hash -r 2>/dev/null || true
  command -v curl >/dev/null 2>&1 || die "自动安装后仍缺少 curl"
  { command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; } || die "自动安装后仍缺少 gcc"
  command -v make >/dev/null 2>&1 || die "自动安装后仍缺少 make"
  command -v perl >/dev/null 2>&1 || die "自动安装后仍缺少 perl"
}

version_ge() {
  [ "$1" = "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n1)" ]
}

ensure_rust() {
  if ! command -v cargo >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; then
    if [ -f "$HOME/.cargo/env" ]; then
      # shellcheck disable=SC1091
      . "$HOME/.cargo/env"
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    [ "$OFFLINE" -eq 0 ] || die "缺少 cargo，且当前为 --offline，无法自动安装 Rust"
    info "未检测到 Rust，安装 rustup（minimal profile）..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    if [ -f "$HOME/.cargo/env" ]; then
      # shellcheck disable=SC1091
      . "$HOME/.cargo/env"
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
    hash -r 2>/dev/null || true
  fi
  command -v cargo >/dev/null 2>&1 || die "rustup 安装失败，请手动检查 cargo"
  command -v rustc >/dev/null 2>&1 || die "缺少 rustc，请安装完整 Rust 工具链"

  local version
  version="$(rustc --version | awk '{print $2}')"
  if ! version_ge "$version" "$RUST_MIN"; then
    if command -v rustup >/dev/null 2>&1; then
      info "Rust $version 低于 $RUST_MIN，升级 stable"
      rustup update stable
      rustup default stable
      version="$(rustc --version | awk '{print $2}')"
    fi
    version_ge "$version" "$RUST_MIN" || die "Rust $version 过低，需要 $RUST_MIN+"
  fi
  ok "Rust $version（要求 $RUST_MIN+）"
}

# --from-settings 依赖主服务二进制读取数据库；必须先于构建执行。
if [ "$FROM_SETTINGS" -eq 1 ] && [ -z "$SECRET" ]; then
  NANOQUANT_BIN="$REPO_ROOT/rust/nanoquant/target/release/nanoquant"
  [ -x "$NANOQUANT_BIN" ] || die "未找到 $NANOQUANT_BIN：请先构建 Rust 后端"
  info "尝试从主服务读取当前交易环境的交易所 Secret"
  SECRET="$("$NANOQUANT_BIN" ops show-exec-secret 2>/dev/null || true)"
  [ -n "$SECRET" ] || die "无法读取当前环境的 Secret，请改用 --secret <SECRET> 显式传入"
  ok "已取到当前环境的 Secret（长度 ${#SECRET}，不打印内容）"
fi

if [ "$SKIP_DEPS" -eq 0 ]; then
  ensure_core_deps
fi

CORE_SRC="$REPO_ROOT/rust/exec-core"
CORE_BIN="$CORE_SRC/target/release/exec-core"
CARGO_FLAGS=()
[ "$OFFLINE" -eq 1 ] && CARGO_FLAGS+=(--offline)
if [ "$SKIP_BUILD" -eq 1 ]; then
  [ -x "$CORE_BIN" ] || die "--skip-build 但未找到可执行文件: $CORE_BIN"
  ok "复用已编译二进制：$CORE_BIN"
else
  [ -d "$CORE_SRC" ] || die "找不到 $CORE_SRC，请在仓库根目录运行本脚本"
  if [ "$SKIP_DEPS" -eq 0 ]; then
    ensure_rust
  fi
  info "编译 exec-core（cargo test --release --locked + build）..."
  ( cd "$CORE_SRC" && cargo test --release --locked "${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}" && cargo build --release --locked "${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}" )
  [ -x "$CORE_BIN" ] || die "编译后未找到二进制: $CORE_BIN"
  ok "编译完成：$CORE_BIN（$(du -h "$CORE_BIN" | cut -f1)）"
fi

install -m 0755 "$CORE_BIN" "$APP_HOME/exec-core"
ok "二进制已安装：$APP_HOME/exec-core"

ENV_FILE=""
if [ "$MODE" = "systemd" ]; then
  ENV_FILE="/etc/nanoquant/exec.env"
  install -d -m 0700 /etc/nanoquant
else
  ENV_FILE="$APP_HOME/data/exec.env"
  if [ -d "$APP_HOME/data" ]; then
    chmod 700 "$APP_HOME/data" 2>/dev/null || warn "无法收紧 data 目录权限: $APP_HOME/data"
  else
    install -d -m 0700 "$APP_HOME/data"
  fi
fi
if [ -n "$SECRET" ]; then
  install -m 600 /dev/null "$ENV_FILE"
  printf 'EXEC_SECRET=%s\n' "$SECRET" >"$ENV_FILE"
  chmod 600 "$ENV_FILE"
  if [ "$MODE" = "systemd" ]; then
    chown root:root "$ENV_FILE" 2>/dev/null || true
  elif [ -n "$RUN_USER" ] && id -u "$RUN_USER" >/dev/null 2>&1; then
    chown "$RUN_USER:$(id -gn "$RUN_USER")" "$ENV_FILE" 2>/dev/null || true
  fi
  ok "已写入 Secret：$ENV_FILE（0600，长度 ${#SECRET}）"
fi
HAS_SECRET=0
if [ -f "$ENV_FILE" ] && [ -s "$ENV_FILE" ]; then HAS_SECRET=1; fi
if [ "$HAS_SECRET" -eq 0 ]; then
  warn "未提供 --secret/--from-settings；exec-core 已安装但不会启动"
fi

if [ "$MODE" = "systemd" ]; then
  if [ -z "$RUN_USER" ] && id -u nanoquant >/dev/null 2>&1; then
    RUN_USER="nanoquant"
  fi
  if [ -n "$RUN_USER" ]; then
    id -u "$RUN_USER" >/dev/null 2>&1 || die "系统用户不存在: $RUN_USER"
    RUN_GROUP="$(id -gn "$RUN_USER")"
    ok "exec-core 运行用户: $RUN_USER:$RUN_GROUP"
  fi
fi

grant_core_access() {
  [ "$MODE" = "systemd" ] || return 0
  [ -n "$RUN_USER" ] || return 0
  [ "$RUN_USER" = "root" ] && return 0
  command -v runuser >/dev/null 2>&1 || {
    warn "缺少 runuser，跳过 exec-core 路径访问预检"
    return 0
  }
  local bin="$APP_HOME/exec-core"
  if runuser -u "$RUN_USER" -- test -x "$bin" >/dev/null 2>&1; then
    return 0
  fi
  warn "exec-core 用户 $RUN_USER 无法执行 $bin，自动授予路径访问权限"
  local path
  path="$(dirname "$bin")"
  while [ "$path" != "/" ]; do
    if ! runuser -u "$RUN_USER" -- test -x "$path" >/dev/null 2>&1; then
      if command -v setfacl >/dev/null 2>&1 \
        && setfacl -m "u:$RUN_USER:x" "$path" 2>/dev/null; then
        info "ACL 授权 $RUN_USER 穿越 $path"
      else
        chmod o+x "$path" 2>/dev/null || die "无法授予 $RUN_USER 穿越 $path"
        warn "已回退 chmod o+x：$path"
      fi
    fi
    path="$(dirname "$path")"
  done
  if ! runuser -u "$RUN_USER" -- test -x "$bin" >/dev/null 2>&1; then
    if command -v setfacl >/dev/null 2>&1 \
      && setfacl -m "u:$RUN_USER:rX" "$bin" 2>/dev/null; then
      info "ACL 授权 $RUN_USER 执行 $bin"
    else
      chmod o+rx "$bin" 2>/dev/null || die "无法授予 $RUN_USER 执行 $bin"
      warn "已回退 chmod o+rx：$bin"
    fi
  fi
  runuser -u "$RUN_USER" -- test -x "$bin" >/dev/null 2>&1 \
    || die "exec-core 用户 $RUN_USER 仍无法执行 $bin"
  ok "exec-core 用户 $RUN_USER 已可执行安装目录中的二进制"
}

if [ "$MODE" = "systemd" ]; then
  cat >"$UNIT_PATH" <<EOF
[Unit]
Description=NanoQuant exec-core (default execution kernel)
Documentation=https://github.com/qiuyuec-qyacgcom/nanoquant/blob/main/docs/EXEC_PROTOCOL.md
After=network-online.target
Wants=network-online.target
Before=nanoquant.service
# 没写密钥就不启动（而不是启动后 panic 退出、被 Restart=always 无限拉起）。
ConditionPathExists=/etc/nanoquant/exec.env

[Service]
Type=simple
RuntimeDirectory=nanoquant
RuntimeDirectoryMode=0750
Environment=EXEC_SOCKET=/run/nanoquant/exec.sock
EnvironmentFile=-/etc/nanoquant/exec.env
ExecStart=$APP_HOME/exec-core
# 78=EX_CONFIG（缺 EXEC_SECRET）：不给它无限重试的机会
RestartPreventExitStatus=78
Restart=always
RestartSec=3

NoNewPrivileges=true
PrivateTmp=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_UNIX
RestrictNamespaces=true
LockPersonality=true
RestrictRealtime=true
RestrictSUIDSGID=true
SystemCallArchitectures=native
# 0077 → socket 文件 0600，只有同用户进程能连
UMask=0077

[Install]
WantedBy=multi-user.target
EOF
  if [ -n "$RUN_USER" ]; then
    if grep -q '^User=' "$UNIT_PATH"; then
      sed -i "s/^User=.*/User=$RUN_USER/" "$UNIT_PATH"
    else
      sed -i "/^\\[Service\\]/a User=$RUN_USER" "$UNIT_PATH"
    fi
    if grep -q '^Group=' "$UNIT_PATH"; then
      sed -i "s/^Group=.*/Group=$RUN_GROUP/" "$UNIT_PATH"
    else
      sed -i "/^User=$RUN_USER$/a Group=$RUN_GROUP" "$UNIT_PATH"
    fi
    chown "$RUN_USER:$RUN_GROUP" "$APP_HOME/exec-core" 2>/dev/null || true
    chmod 0755 "$APP_HOME/exec-core"
  fi
  grant_core_access
  systemctl daemon-reload
  systemctl enable "$UNIT_NAME" >/dev/null
  if [ "$NO_START" -eq 1 ]; then
    ok "unit 已安装并设为开机启动；按 --no-start 未启动/重启"
    warn "运行中的旧进程（如有）仍在用旧二进制；需要时执行: systemctl restart $UNIT_NAME"
    exit 0
  fi
  if [ "$HAS_SECRET" -eq 0 ]; then
    ok "unit 已安装并设为开机启动；配置 Secret 后再启动"
    echo
    echo "下一步："
    echo "  1) 在后台「参数配置 → 交易所」保存 API 凭证"
    echo "  2) sudo bash scripts/setup_core.sh --from-settings"
    echo "  3) sudo systemctl restart nanoquant"
    exit 0
  fi
  info "启动 exec-core"
  systemctl restart "$UNIT_NAME" || true
  sleep 1
  if systemctl is-active --quiet "$UNIT_NAME"; then
    ok "exec-core 已启动"
    echo
    echo "主服务需要重新握手；如果它已经在运行，请重启："
    echo "  sudo systemctl restart nanoquant"
  else
    warn "exec-core 未激活：journalctl -u $UNIT_NAME -n 30 --no-pager"
    exit 1
  fi
  exit 0
fi

ok "exec-core 已安装（nohup 模式）：$APP_HOME/exec-core"
if [ "$HAS_SECRET" -eq 1 ]; then
  echo
  echo "下一步：bash scripts/start.sh --no-systemd"
else
  echo
  echo "下一步："
  echo "  1) 在后台「参数配置 → 交易所」保存 API 凭证"
  echo "  2) bash scripts/setup_core.sh --no-systemd --from-settings"
  echo "  3) bash scripts/start.sh --no-systemd"
fi
