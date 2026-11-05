#!/usr/bin/env bash
# NanoQuant 一键安装脚本 (类宝塔面板式交互)
#
#   sudo bash scripts/install.sh                 # 默认端口 8899, 安装到当前目录
#   sudo bash scripts/install.sh --port 9000     # 指定端口
#   sudo bash scripts/install.sh --systemd       # 同时注册 systemd 开机自启
#   sudo bash scripts/install.sh --no-frontend   # 跳过前端构建 (已有 dist)
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${NANOQUANT_PORT:-8899}"
WITH_SYSTEMD=0
WITH_FRONTEND=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --port) PORT="$2"; shift 2 ;;
    --systemd) WITH_SYSTEMD=1; shift ;;
    --no-frontend) WITH_FRONTEND=0; shift ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1"; exit 1 ;;
  esac
done

C_RESET='\033[0m'; C_OK='\033[32m'; C_WARN='\033[33m'; C_ERR='\033[31m'; C_INFO='\033[36m'
info() { printf "${C_INFO}[信息]${C_RESET} %s\n" "$*"; }
ok()   { printf "${C_OK}[完成]${C_RESET} %s\n" "$*"; }
warn() { printf "${C_WARN}[注意]${C_RESET} %s\n" "$*"; }
die()  { printf "${C_ERR}[失败]${C_RESET} %s\n" "$*"; exit 1; }

banner() {
  cat <<'EOF'
 _   _             ___                      _
| \ | | __ _ _ __ / _ \ _   _  __ _ _ __   | |_
|  \| |/ _` | '_ \ | | | | | |/ _` | '_ \  | __|
| |\  | (_| | | | | |_| | |_| | (_| | | | | |_
|_| \_|\__,_|_| |_|\__\_\\__,_|\__,_|_| |_|\__|

  小资金 · 币安 U 本位永续 · 超短线量化终端
EOF
}

banner
info "安装目录: $ROOT"
info "监听端口: $PORT"

# ---------------------------------------------------------------- 依赖检查
PKG=""
if command -v apt-get >/dev/null 2>&1; then PKG="apt"
elif command -v dnf >/dev/null 2>&1; then PKG="dnf"
elif command -v yum >/dev/null 2>&1; then PKG="yum"
elif command -v apk >/dev/null 2>&1; then PKG="apk"
fi

install_pkg() {
  [[ -z "$PKG" ]] && return 1
  case "$PKG" in
    apt) DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" ;;
    dnf) dnf install -y -q "$@" ;;
    yum) yum install -y -q "$@" ;;
    apk) apk add --no-cache "$@" ;;
  esac
}

# Python 3.10+
PY=""
for cand in python3.12 python3.11 python3.10 python3; do
  if command -v "$cand" >/dev/null 2>&1; then
    if "$cand" -c 'import sys; raise SystemExit(0 if sys.version_info >= (3,10) else 1)' 2>/dev/null; then
      PY="$cand"; break
    fi
  fi
done

if [[ -z "$PY" ]]; then
  warn "未找到 Python 3.10+, 尝试自动安装"
  if [[ "$PKG" == "apt" ]]; then
    apt-get update -qq && install_pkg python3 python3-venv python3-pip
  else
    install_pkg python3 python3-pip
  fi
  PY=python3
fi
ok "Python: $($PY --version 2>&1)"

# venv 模块
if ! "$PY" -c 'import venv' 2>/dev/null; then
  warn "缺少 venv 模块, 尝试安装"
  [[ "$PKG" == "apt" ]] && install_pkg python3-venv
fi

# ---------------------------------------------------------------- 虚拟环境
VENV="$ROOT/.venv"
if [[ ! -x "$VENV/bin/python" ]]; then
  info "创建虚拟环境 $VENV"
  "$PY" -m venv "$VENV" || die "venv 创建失败, 请安装 python3-venv"
fi
VPY="$VENV/bin/python"
info "安装后端依赖 (首次可能需要几分钟)"
"$VPY" -m pip install --upgrade pip -q
"$VPY" -m pip install -q -r "$ROOT/backend/requirements.txt" || die "后端依赖安装失败, 请检查网络或配置国内镜像"
ok "后端依赖就绪"

# ---------------------------------------------------------------- 前端构建
if [[ "$WITH_FRONTEND" == "1" ]]; then
  if [[ -f "$ROOT/frontend/dist/index.html" ]]; then
    info "检测到已有前端产物, 跳过构建 (如需重建请删除 frontend/dist)"
  else
    if command -v npm >/dev/null 2>&1; then
      info "构建前端 (npm install + build)"
      ( cd "$ROOT/frontend" && npm install --no-audit --no-fund && npm run build ) \
        || die "前端构建失败"
      ok "前端构建完成"
    else
      warn "未检测到 Node.js/npm, 跳过前端构建"
      warn "可稍后安装 Node 18+ 后执行: cd frontend && npm install && npm run build"
    fi
  fi
fi

# ---------------------------------------------------------------- 运行目录
mkdir -p "$ROOT/data" "$ROOT/logs" "$ROOT/data/backups"
chmod 700 "$ROOT/data" 2>/dev/null || true

# ---------------------------------------------------------------- systemd
if [[ "$WITH_SYSTEMD" == "1" ]]; then
  if [[ ! -d /etc/systemd/system ]]; then
    warn "当前系统没有 systemd, 跳过服务注册"
  else
    SERVICE=/etc/systemd/system/nanoquant.service
    cat > "$SERVICE" <<EOF
[Unit]
Description=NanoQuant 小资金合约量化终端
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$ROOT/backend
Environment=NANOQUANT_HOME=$ROOT
Environment=NANOQUANT_PORT=$PORT
ExecStart=$VENV/bin/python -m nanoquant
Restart=always
RestartSec=5
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable nanoquant >/dev/null 2>&1 || true
    systemctl restart nanoquant
    ok "已注册并启动 systemd 服务: systemctl status nanoquant"
  fi
fi

IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
IP="${IP:-127.0.0.1}"

cat <<EOF

$(printf "${C_OK}========================================${C_RESET}")
 安装完成
$(printf "${C_OK}========================================${C_RESET}")

  访问地址 : http://$IP:$PORT
  默认账号 : admin
  默认密码 : admin123   ${C_WARN}(登录后请立即修改)${C_RESET}

  启动     : bash scripts/start.sh
  停止     : bash scripts/stop.sh
  查看状态 : bash scripts/status.sh

  数据目录 : $ROOT/data     (SQLite / 备份 / 缓存)
  日志目录 : $ROOT/logs
  配置文件 : 全部在面板「参数配置」中, 存于数据库并可热更新

  首次使用建议顺序:
   1. 「参数配置 → 数据源」填写币安 API Key (仅开合约权限, 关闭提币)
   2. 「AI 模型」接入 DeepSeek / Qwen / GPT 等 OpenAI 兼容模型并点「诊断」
   3. 「实盘矩阵」观察信号 → 「总览」先以 DEMO 模拟盘模式启动
   4. 确认逻辑后再切换到 LIVE 实盘

EOF
