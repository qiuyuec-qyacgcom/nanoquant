#!/usr/bin/env bash
# 部署 Rust 执行内核 sidecar（sign-only）到 Linux VPS（Debian/Ubuntu）
#
# sidecar 通过 UDS(/run/nanoquant/exec.sock) 与主服务通信，与主服务所在目录无关。
# 与 install.sh 一样「就地部署」：默认把二进制装到仓库根目录（clone 目录）下，
# 不依赖 /opt，主服务与 sidecar 共用同一个克隆出来的目录。
#
# 用法（在 git clone 的仓库根目录，root 权限）：
#   sudo bash scripts/setup_sidecar.sh --secret '<交易所签名密钥>'
#
# --secret  必填：与 Python 侧交易所 API secret 是**同一把密钥**
#            （Ed25519 私钥的 base64 PKCS#8，或 HMAC secret）
#            也可以不传，部署后手动写 /etc/nanoquant/exec.env（权限 0600）
# --from-settings  从主服务数据库里读**当前交易环境**的交易所 Secret（最省事）：
#            面板填好 API 凭证后执行一次即可，不必再手工复制密钥
# --skip-build     二进制已存在时跳过编译（install.sh 自动调用时用）
# --home    二进制安装目录，默认仓库根目录（clone 目录）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APP_HOME="$REPO_ROOT"
SECRET=""
FROM_SETTINGS=0
SKIP_BUILD=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --secret) SECRET="${2:-}"; shift 2 ;;
    --from-settings) FROM_SETTINGS=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --home)   APP_HOME="${2:-$REPO_ROOT}"; shift 2 ;;
    *) echo "未知参数: $1（支持 --secret / --from-settings / --skip-build / --home）"; exit 1 ;;
  esac
done

info() { printf '\033[1;34m[sidecar]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[sidecar]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[sidecar]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[sidecar]\033[0m %s\n' "$*" >&2; exit 1; }

[[ $(id -u) -eq 0 ]] || die "需要 root：sudo bash scripts/setup_sidecar.sh ..."

# ---------- 0. --from-settings：直接从数据库读当前环境的交易所 Secret ----------
if [[ "$FROM_SETTINGS" == "1" && -z "$SECRET" ]]; then
  VENV_PY="$REPO_ROOT/.venv/bin/python"
  [[ -x "$VENV_PY" ]] || die "未找到 $VENV_PY：请先跑 install.sh 建好虚拟环境"
  info "从主服务数据库读取当前交易环境的交易所 Secret"
  SECRET="$(cd "$REPO_ROOT/backend" && "$VENV_PY" - <<'PY'
import sys
try:
    from nanoquant.services.settings import get_data, get_engine, normalize_mode
    from nanoquant.security import decrypt_secret
except Exception as exc:  # noqa: BLE001
    print(f"__ERR__{exc}", file=sys.stderr)
    raise SystemExit(1)
cfg = get_data()
env = normalize_mode(get_engine().mode)
secret = decrypt_secret(
    cfg.demo_api_secret_enc if env == "demo" else cfg.live_api_secret_enc
)
if not secret:
    print(f"__ERR__当前环境（{env}）未配置 API Secret", file=sys.stderr)
    raise SystemExit(1)
print(secret)
PY
)" || die "读取失败：请先在面板「参数配置 → 数据源」填好该环境的 API Key/Secret"
  [[ -n "$SECRET" ]] || die "数据库里的 API Secret 为空"
  ok "已取到当前环境的 Secret（长度 ${#SECRET}，不打印内容）"
fi

# ---------- 1. Rust 工具链 ----------
BIN="$REPO_ROOT/rust/exec-service/target/release/exec-service"
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
  SRC_DIR="$REPO_ROOT/rust/exec-service"
  [[ -d "$SRC_DIR" ]] || die "找不到 $SRC_DIR，请在仓库根目录运行本脚本"
  cd "$SRC_DIR"
  info "编译 sidecar（cargo test + build --release）..."
  cargo test --release || die "cargo test 失败"
  cargo build --release || die "cargo build 失败"
  ok "编译完成：$BIN（$(du -h "$BIN" | cut -f1)）"
fi

# ---------- 3. 安装二进制 ----------
info "安装二进制到 $APP_HOME/exec-service"
install -m 0755 "$BIN" "$APP_HOME/exec-service"
ok "二进制已安装"

# ---------- 4. Secret（必须与 Python 侧交易所签名密钥一致）----------
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
  warn "  sudo bash scripts/setup_sidecar.sh --from-settings"
fi

# ---------- 5. systemd ----------
# 就地部署：unit 动态生成，ExecStart 指向 $APP_HOME/exec-service，与 install.sh 主服务同一套目录。
# 与主服务一样以 root 运行（install.sh 未创建 nanoquant 系统用户），故不设 User=。
SERVICE=/etc/systemd/system/nanoquant-exec.service
info "注册 systemd unit：nanoquant-exec.service（ExecStart=$APP_HOME/exec-service）"
cat > "$SERVICE" <<EOF
[Unit]
Description=NanoQuant exec-service (Rust signing sidecar, sign-only)
Documentation=https://github.com/qiuyuec-qyacgcom/nanoquant/blob/main/docs/EXEC_PROTOCOL.md
After=network-online.target
Wants=network-online.target
Before=nanoquant.service
# 没写密钥就不启动（而不是启动后 panic 退出、被 Restart=always 无限拉起）。
# 填好面板 API 凭证后执行 setup_sidecar.sh --from-settings 即会创建该文件。
ConditionPathExists=/etc/nanoquant/exec.env

[Service]
Type=simple
# /run/nanoquant 由 systemd 创建，属主随运行用户（root）
RuntimeDirectory=nanoquant
RuntimeDirectoryMode=0750
Environment=EXEC_SOCKET=/run/nanoquant/exec.sock
EnvironmentFile=-/etc/nanoquant/exec.env
ExecStart=$APP_HOME/exec-service
# 78=EX_CONFIG（缺 EXEC_SECRET）：不给它无限重试的机会
RestartPreventExitStatus=78
Restart=always
RestartSec=3

NoNewPrivileges=true
PrivateTmp=true
# 0077 → socket 文件 0600，只有同用户进程能连
UMask=0077

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now nanoquant-exec
sleep 1
if systemctl is-active nanoquant-exec >/dev/null 2>&1; then
  ok "sidecar 已启动"
else
  warn "sidecar 未激活：journalctl -u nanoquant-exec -n 30 --no-pager"
fi

echo
ok "sidecar 部署完成"
cat <<EOF

接下来（按顺序）：
  1) 确认 socket：ls -l /run/nanoquant/exec.sock
  2) 逐字节比对（与 Python 侧同密钥）：
       EXEC_SECRET='<密钥>' $APP_HOME/.venv/bin/python $APP_HOME/scripts/verify_sidecar.py --target unix:/run/nanoquant/exec.sock
  3) 后台「系统诊断 → 执行内核」：确认 target=unix:/run/nanoquant/exec.sock、connected=true
  4) 点「自检」→ 两侧 88 字符 base64 一致后，exec_mode 先设 shadow 观察 → mismatched 恒 0 再切 enforce（灰度 24h）
  5) 换 Secret 后必须点「清零」，否则切换窗口内会残留不一致计数
EOF
