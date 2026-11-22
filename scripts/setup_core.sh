#!/usr/bin/env bash
# 部署 Rust 执行内核 exec-core 到 Linux VPS（Debian/Ubuntu）
#
# exec-core 是默认执行内核（签名校验 + 七段指标聚合），通过 UDS
# (/run/nanoquant/exec.sock) 与主服务通信；主服务启动时完成握手，否则不启动交易引擎。
#
# 用法（在 git clone 的仓库根目录，root 权限）：
#   sudo bash scripts/setup_core.sh --secret '<交易所签名密钥>'
#
# --secret  必填：与交易所 API secret 是同一把密钥（Ed25519 私钥 base64 PKCS#8，或 HMAC secret）
# --from-settings  从主服务数据库读当前交易环境的交易所 Secret（最省事）
# --skip-build     二进制已存在时跳过编译
# --home    二进制安装目录，默认仓库根目录（clone 目录）
# --user    systemd 运行用户；默认复用已存在的 nanoquant 系统用户，否则 root
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APP_HOME="$REPO_ROOT"
SECRET=""
FROM_SETTINGS=0
SKIP_BUILD=0
RUN_USER="${NANOQUANT_USER:-}"
RUN_GROUP=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --secret) SECRET="${2:-}"; shift 2 ;;
    --from-settings) FROM_SETTINGS=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --home)   APP_HOME="${2:-$REPO_ROOT}"; shift 2 ;;
    --user)   RUN_USER="${2:-}"; shift 2 ;;
    *) echo "未知参数: $1（支持 --secret / --from-settings / --skip-build / --home / --user）"; exit 1 ;;
  esac
done

info() { printf '\033[1;34m[exec-core]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[exec-core]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[exec-core]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[exec-core]\033[0m %s\n' "$*" >&2; exit 1; }

[[ $(id -u) -eq 0 ]] || die "需要 root：sudo bash scripts/setup_core.sh ..."

ensure_core_deps() {
  local missing=()
  local pm=""
  if command -v apt-get >/dev/null 2>&1; then pm=apt
  elif command -v dnf >/dev/null 2>&1; then pm=dnf
  elif command -v yum >/dev/null 2>&1; then pm=yum
  elif command -v apk >/dev/null 2>&1; then pm=apk
  elif command -v zypper >/dev/null 2>&1; then pm=zypper
  elif command -v pacman >/dev/null 2>&1; then pm=pacman
  fi
  command -v curl >/dev/null 2>&1 || missing+=("curl")
  if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1; then missing+=("gcc"); fi
  command -v make >/dev/null 2>&1 || missing+=("make")
  command -v perl >/dev/null 2>&1 || missing+=("perl")
  command -v pkg-config >/dev/null 2>&1 || missing+=("pkg-config")
  command -v setfacl >/dev/null 2>&1 || missing+=("setfacl")
  [ "${#missing[@]}" -gt 0 ] || return 0
  [ -n "$pm" ] || die "缺少 ${missing[*]}，且未识别包管理器；无法自动安装 exec-core 构建依赖"
  info "自动安装 exec-core 构建依赖（$pm）：${missing[*]}"
  case "$pm" in
    apt)
      env DEBIAN_FRONTEND=noninteractive apt-get update -qq
      env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        gcc libc6-dev make perl curl ca-certificates pkg-config acl
      ;;
    dnf) dnf install -y gcc gcc-c++ glibc-devel make perl curl ca-certificates pkgconf-pkg-config acl ;;
    yum) yum install -y gcc gcc-c++ glibc-devel make perl curl ca-certificates pkgconfig acl ;;
    apk) apk add --no-cache gcc musl-dev make perl curl ca-certificates pkgconf acl ;;
    zypper) zypper --non-interactive install gcc gcc-c++ glibc-devel make perl curl ca-certificates pkg-config acl ;;
    pacman) pacman -Sy --needed --noconfirm gcc make perl curl ca-certificates pkgconf acl ;;
  esac
  hash -r 2>/dev/null || true
  command -v curl >/dev/null 2>&1 || die "自动安装后仍缺少 curl"
  { command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; } || die "自动安装后仍缺少 gcc"
  command -v make >/dev/null 2>&1 || die "自动安装后仍缺少 make"
  command -v perl >/dev/null 2>&1 || die "自动安装后仍缺少 perl"
  command -v setfacl >/dev/null 2>&1 || die "自动安装后仍缺少 setfacl"
}

ensure_core_deps
command -v systemctl >/dev/null 2>&1 || die "缺少 systemctl，无法注册 exec-core 服务"

if [[ -z "$RUN_USER" ]] && id -u nanoquant >/dev/null 2>&1; then
  RUN_USER="nanoquant"
fi
if [[ -n "$RUN_USER" ]]; then
  id -u "$RUN_USER" >/dev/null 2>&1 || die "系统用户不存在: $RUN_USER"
  RUN_GROUP="$(id -gn "$RUN_USER")"
  ok "exec-core 运行用户: $RUN_USER:$RUN_GROUP"
fi

# ---------- 0. --from-settings：直接从数据库读当前环境的交易所 Secret ----------
if [[ "$FROM_SETTINGS" == "1" && -z "$SECRET" ]]; then
  NANOQUANT_BIN="$REPO_ROOT/rust/nanoquant/target/release/nanoquant"
  [[ -x "$NANOQUANT_BIN" ]] || die "未找到 $NANOQUANT_BIN：请先构建 Rust 后端（cargo build --release --locked --manifest-path rust/nanoquant/Cargo.toml）"
  info "尝试从主服务读取当前交易环境的交易所 Secret"
  SECRET="$("$NANOQUANT_BIN" ops show-exec-secret 2>/dev/null || true)"
  [[ -n "$SECRET" ]] || die "当前二进制不支持读取 Secret，请改用 --secret <SECRET> 显式传入"
  ok "已取到当前环境的 Secret（长度 ${#SECRET}，不打印内容）"
fi

# ---------- 1. Rust 工具链 ----------
BIN="$REPO_ROOT/rust/exec-core/target/release/exec-core"
if [[ "$SKIP_BUILD" == "1" && -x "$BIN" ]]; then
  ok "复用已编译二进制：$BIN"
else
  if command -v cargo >/dev/null 2>&1; then
    ok "Rust 已就绪：$(cargo --version)"
  else
    info "未检测到 Rust，安装 rustup（minimal profile）..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
    command -v cargo >/dev/null 2>&1 || die "rustup 安装失败，请手动检查"
    ok "Rust 安装完成：$(cargo --version)"
  fi

  # 2. 编译（Linux 产物）
  SRC_DIR="$REPO_ROOT/rust/exec-core"
  [[ -d "$SRC_DIR" ]] || die "找不到 $SRC_DIR，请在仓库根目录运行本脚本"
  cd "$SRC_DIR"
  info "编译 exec-core（cargo test + build --release）..."
  cargo test --release || die "cargo test 失败"
  cargo build --release || die "cargo build 失败"
  ok "编译完成：$BIN（$(du -h "$BIN" | cut -f1)）"
fi

# ---------- 3. 安装二进制 ----------
info "安装二进制到 $APP_HOME/exec-core"
install -m 0755 "$BIN" "$APP_HOME/exec-core"
ok "二进制已安装"

grant_core_access() {
  [[ -n "$RUN_USER" ]] || return 0
  [[ "$RUN_USER" == "root" ]] && return 0
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
  while [[ "$path" != "/" ]]; do
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

# ---------- 4. Secret（必须与交易所签名密钥一致）----------
mkdir -p /etc/nanoquant
if [[ -n "$SECRET" ]]; then
  # 先建 0600 空文件再写内容：`umask` 管不住别的进程，靠 install+chmod 最稳
  install -m 600 -o root -g root /dev/null /etc/nanoquant/exec.env
  printf 'EXEC_SECRET=%s\n' "$SECRET" > /etc/nanoquant/exec.env
  chmod 600 /etc/nanoquant/exec.env
  chown root:root /etc/nanoquant/exec.env 2>/dev/null || true
  ok "已写入 /etc/nanoquant/exec.env（0600，长度 ${#SECRET}）"
else
  warn "未提供 --secret，跳过写入。填好面板 API 凭证后执行一次："
  warn "  sudo bash scripts/setup_core.sh --from-settings"
fi

# ---------- 5. systemd ----------
# 就地部署：unit 动态生成，ExecStart 指向 $APP_HOME/exec-core，与 install.sh 主服务同一套目录。
SERVICE=/etc/systemd/system/nanoquant-exec-core.service
info "注册 systemd unit：nanoquant-exec-core.service（ExecStart=$APP_HOME/exec-core）"
cat > "$SERVICE" <<EOF
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
if [[ -n "$RUN_USER" ]]; then
  sed -i "/^\[Service\]/a User=$RUN_USER" "$SERVICE"
  sed -i "/^User=$RUN_USER$/a Group=$RUN_GROUP" "$SERVICE"
  chown "$RUN_USER:$RUN_GROUP" "$APP_HOME/exec-core"
  chmod 0755 "$APP_HOME/exec-core"
fi
grant_core_access
systemctl daemon-reload
systemctl enable --now nanoquant-exec-core
sleep 1
if systemctl is-active nanoquant-exec-core >/dev/null 2>&1; then
  ok "exec-core 已启动"
else
  warn "exec-core 未激活：journalctl -u nanoquant-exec-core -n 30 --no-pager"
fi

echo
ok "exec-core 部署完成"
cat <<EOF

接下来（按顺序）：
  1) 确认 socket：ls -l /run/nanoquant/exec.sock
  2) 逐字节比对（与主服务自己的签名对齐）：
       EXEC_SECRET='<密钥>' $APP_HOME/rust/nanoquant/target/release/nanoquant ops verify-exec-core --target unix:/run/nanoquant/exec.sock
  3) 后台「系统诊断 → 执行内核（exec-core）」：确认 target=unix:/run/nanoquant/exec.sock、connected=true
  4) 重启主服务：systemctl restart nanoquant（启动握手失败时交易引擎不启动）
  5) 点「自检」：面板会记下探活一帧 health 的实测往返耗时
  6) 换 Secret 后点「清零」，清掉上一把密钥留下的观测计数
EOF