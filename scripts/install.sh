#!/usr/bin/env bash
# NanoQuant 一键安装脚本
#
#   sudo bash scripts/install.sh                 # 默认端口 8899
#   sudo bash scripts/install.sh --port 9000     # 指定端口（写进 data/.port，start.sh 沿用）
#   sudo bash scripts/install.sh --systemd       # 注册 systemd 自启，并等健康检查通过
#   sudo bash scripts/install.sh --no-frontend   # 跳过前端构建（已有 dist 或只跑后端）
#   sudo bash scripts/install.sh --mirror cn     # 用国内 pip 镜像（清华源）
#   sudo bash scripts/install.sh --force         # 强制重装依赖（默认按 requirements 哈希跳过）
#
# 设计取舍：
# - 幂等：重复执行不会每次重装依赖/重建 venv，改完 requirements 再跑一次即可；
# - 失败可见：任何一步失败都以非 0 退出并说明下一步，绝不打印"安装完成"；
# - 不自作主张：缺 Node 只警告不中止（后端可独立跑），Python 版本不够才中止。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${NANOQUANT_PORT:-8899}"
WITH_SYSTEMD=0
WITH_FRONTEND=1
FORCE=0
MIRROR="${PIP_INDEX_URL:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --port)
      PORT="${2:-}"
      [[ "$PORT" =~ ^[0-9]+$ ]] || { echo "--port 需要一个数字"; exit 1; }
      shift 2 ;;
    --systemd) WITH_SYSTEMD=1; shift ;;
    --no-frontend) WITH_FRONTEND=0; shift ;;
    --mirror)
      case "${2:-}" in
        cn|tsinghua) MIRROR="https://pypi.tuna.tsinghua.edu.cn/simple" ;;
        aliyun) MIRROR="https://mirrors.aliyun.com/pypi/simple" ;;
        *) MIRROR="${2:-}" ;;
      esac
      shift 2 ;;
    --force) FORCE=1; shift ;;
    -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1（用 --help 查看用法）"; exit 1 ;;
  esac
done

C_RESET='\033[0m'; C_OK='\033[32m'; C_WARN='\033[33m'; C_ERR='\033[31m'; C_INFO='\033[36m'
info() { printf "${C_INFO}[信息]${C_RESET} %s\n" "$*"; }
ok()   { printf "${C_OK}[完成]${C_RESET} %s\n" "$*"; }
warn() { printf "${C_WARN}[注意]${C_RESET} %s\n" "$*"; }
die()  { printf "${C_ERR}[失败]${C_RESET} %s\n" "$*" >&2; exit 1; }

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

# Git Bash / Cygwin 不是真 Linux：venv 是 Scripts\ 布局、也没有 systemd/chmod，
# 在这里"装成功了"只会在后面启动时炸；直接把 Windows 用户送到正确脚本
case "$(uname -s 2>/dev/null || echo unknown)" in
  MINGW*|MSYS*|CYGWIN*)
    printf "${C_ERR}[失败]${C_RESET} 检测到 Git Bash / Cygwin。\n" >&2
    echo "  Windows 请用: powershell -ExecutionPolicy Bypass -File scripts\\install.ps1" >&2
    exit 1 ;;
esac

if [[ "$(id -u)" == "0" ]]; then
  info "以 root 运行：生成的文件属主是 root，后续请用同一用户执行 start.sh"
fi

# ---------------------------------------------------------------- 磁盘空间
# SQLite / 备份 / 日志都在 data/ 下，空间不足会变成"盘中写入失败"，比启动失败更难查
FREE_KB="$(df -Pk "$ROOT" 2>/dev/null | awk 'NR==2 {print $4}')"
if [[ -n "${FREE_KB:-}" && "$FREE_KB" -lt 2097152 ]]; then
  warn "可用空间仅 $((FREE_KB / 1024)) MB（建议 ≥ 2 GB）：备份/日志/SQLite 都写在这里"
fi

# ---------------------------------------------------------------- 包管理器
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

# 与 backend/requirements.txt（uv 按 3.12 编译）和 CI 矩阵保持一致：
# 只在 3.12/3.13 上验证过，版本不够直接拒掉，比"装完了跑不起来"省事
MIN_MINOR=12
PY=""
for cand in python3.13 python3.12 python3; do
  command -v "$cand" >/dev/null 2>&1 || continue
  if "$cand" -c "import sys; raise SystemExit(0 if sys.version_info[:2] >= (3, $MIN_MINOR) else 1)" 2>/dev/null; then
    PY="$cand"; break
  fi
done

if [[ -z "$PY" ]]; then
  warn "未找到 Python 3.12+，尝试自动安装"
  if [[ "$PKG" == "apt" ]]; then
    apt-get update -qq || true
    install_pkg python3 python3-venv python3-pip || true
  else
    install_pkg python3 python3-pip || true
  fi
  for cand in python3.13 python3.12; do
    if command -v "$cand" >/dev/null 2>&1; then PY="$cand"; break; fi
  done
  if [[ -z "$PY" ]] && command -v python3 >/dev/null 2>&1 \
     && python3 -c "import sys; raise SystemExit(0 if sys.version_info[:2] >= (3, $MIN_MINOR) else 1)" 2>/dev/null; then
    PY="python3"
  fi
  [[ -n "$PY" ]] || die "系统源里的 Python 低于 3.12。请先安装 3.12+（deadsnakes / pyenv / 官方源码）后重试"
fi
ok "Python: $($PY --version 2>&1)"

if ! "$PY" -c 'import venv' 2>/dev/null; then
  warn "缺少 venv 模块，尝试安装"
  [[ "$PKG" == "apt" ]] && install_pkg python3-venv
  "$PY" -c 'import venv' 2>/dev/null || die "仍缺少 venv 模块，请安装 python3-venv 后重试"
fi

# ---------------------------------------------------------------- 虚拟环境
VENV="$ROOT/.venv"
# 兼容 Windows 布局（Scripts/python.exe）：仓库放在 Windows 盘上被 WSL 挂进来时
# 常见这种 venv，直接判成"没有 venv"会把它重建一遍
venv_python() {
  if [[ -x "$VENV/bin/python" ]]; then printf '%s\n' "$VENV/bin/python"
  elif [[ -x "$VENV/Scripts/python.exe" ]]; then printf '%s\n' "$VENV/Scripts/python.exe"
  fi
}
VPY="$(venv_python)"
if [[ -z "$VPY" ]]; then
  info "创建虚拟环境 $VENV"
  "$PY" -m venv "$VENV" || die "venv 创建失败（Debian/Ubuntu 需要 python3-venv 包）"
  VPY="$(venv_python)"
  [[ -n "$VPY" ]] || die "venv 创建后仍找不到解释器，请检查 $VENV 目录"
else
  info "复用已有虚拟环境 $VENV"
fi

# ---------------------------------------------------------------- 后端依赖
REQ="$ROOT/backend/requirements.txt"
HASH_FILE="$VENV/.requirements.sha256"
NEW_HASH="$(sha256sum "$REQ" | awk '{print $1}')"
PIP_ARGS=(-q)
[[ -n "$MIRROR" ]] && PIP_ARGS+=(-i "$MIRROR")

if [[ "$FORCE" == "0" && -f "$HASH_FILE" && "$(cat "$HASH_FILE" 2>/dev/null)" == "$NEW_HASH" ]]; then
  info "依赖未变（requirements.txt 哈希一致），跳过安装；要强制重装加 --force"
else
  info "安装后端依赖（首次可能要几分钟）"
  [[ -n "$MIRROR" ]] && info "pip 镜像: $MIRROR"
  "$VPY" -m pip install --upgrade pip "${PIP_ARGS[@]}" >/dev/null 2>&1 || true
  "$VPY" -m pip install "${PIP_ARGS[@]}" -r "$REQ" \
    || die "后端依赖安装失败：可加 --mirror cn 换源重试，或检查网络/代理"
  printf '%s\n' "$NEW_HASH" > "$HASH_FILE"
  ok "后端依赖就绪"
fi

# 装完自检：能 import 才算真的齐（镜像包没下全的情况确实存在）
( cd "$ROOT/backend" && "$VPY" -c "import nanoquant.main" ) \
  || die "依赖装完但 import 失败，请把上面的报错贴出来"
ok "后端可导入（nanoquant.main）"

# ---------------------------------------------------------------- 前端构建
if [[ "$WITH_FRONTEND" == "1" ]]; then
  if [[ -f "$ROOT/frontend/dist/index.html" ]]; then
    info "已存在前端产物，跳过构建（要重建请先删 frontend/dist）"
  elif ! command -v npm >/dev/null 2>&1; then
    warn "未安装 Node.js/npm，跳过前端构建"
    warn "面板打不开但 API 正常；装好 Node 18+ 后执行: cd frontend && npm ci && npm run build"
  else
    NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)"
    if [[ "${NODE_MAJOR:-0}" -lt 18 ]]; then
      warn "Node 版本过低（当前 $(node -v 2>/dev/null)），前端需要 18+，已跳过构建"
    else
      info "构建前端（npm ci + build）"
      if [[ -f "$ROOT/frontend/package-lock.json" ]]; then
        ( cd "$ROOT/frontend" && npm ci --no-audit --no-fund && npm run build ) || die "前端构建失败"
      else
        ( cd "$ROOT/frontend" && npm install --no-audit --no-fund && npm run build ) || die "前端构建失败"
      fi
      ok "前端构建完成（frontend/dist）"
    fi
  fi
fi

# ---------------------------------------------------------------- 运行目录
mkdir -p "$ROOT/data" "$ROOT/logs" "$ROOT/data/backups"
chmod 700 "$ROOT/data" 2>/dev/null || true
# 端口持久化：--port 要真的被 start.sh 沿用，否则只是个"提示里好看"的参数
printf '%s\n' "$PORT" > "$ROOT/data/.port" 2>/dev/null || true
ok "运行目录就绪（data/ logs/ data/backups/）"

# ---------------------------------------------------------------- systemd
if [[ "$WITH_SYSTEMD" == "1" ]]; then
  if [[ ! -d /etc/systemd/system ]]; then
    warn "当前系统没有 systemd，跳过服务注册"
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
    systemctl daemon-reload || die "systemctl daemon-reload 失败（注册服务需要 root）"
    systemctl enable nanoquant >/dev/null 2>&1 || true
    systemctl restart nanoquant || die "服务启动失败：journalctl -u nanoquant -n 50 --no-pager"
    info "等待健康检查（最多 30s）"
    HEALTHY=0
    for _ in $(seq 1 30); do
      if curl -fsS "http://127.0.0.1:$PORT/api/v1/health" >/dev/null 2>&1; then HEALTHY=1; break; fi
      sleep 1
    done
    if [[ "$HEALTHY" == "1" ]]; then
      ok "systemd 服务已启动且健康检查通过（systemctl status nanoquant）"
    else
      warn "服务已注册但健康检查未通过：journalctl -u nanoquant -n 50 --no-pager"
      warn "常见原因：端口被占用 / 依赖缺失 / data 目录权限不足"
    fi
  fi
fi

# hostname -I 不是所有发行版都支持（BusyBox / macOS 都没有），
# set -o pipefail 下失败会直接把脚本整段带走，所以这里必须兜住
IP="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
IP="${IP:-127.0.0.1}"

cat <<EOF

$(printf "${C_OK}========================================${C_RESET}")
 安装完成
$(printf "${C_OK}========================================${C_RESET}")

  访问地址 : http://$IP:$PORT
  默认账号 : admin
  初始密码 : admin123

  $(printf "${C_WARN}初始密码只够登录：改密前后台其它接口一律 403${C_RESET}")
  登录后会被直接送到「参数配置 → 安全」，改完立即恢复。

  启动     : bash scripts/start.sh          （加 --daemon 后台运行）
  停止     : bash scripts/stop.sh
  查看状态 : bash scripts/status.sh
  健康检查 : curl http://127.0.0.1:$PORT/api/v1/health

  数据目录 : $ROOT/data     (SQLite / 备份 / 缓存)
  日志目录 : $ROOT/logs
  参数配置 : 全部在面板里，存于数据库，改完即时生效

  首次使用建议顺序:
   1. 改掉初始密码（系统会强制）
   2. 「参数配置 → 数据源」填币安 API Key（只开合约交易，关闭提币）
   3. 「参数配置 → 系统 → 告警」配 Webhook / Telegram，否则出事只能在页面里看
   4. 「AI 模型」接入 DeepSeek / Qwen / GPT 等 OpenAI 兼容模型并点「诊断」
   5. 「标的池」添加合约 → 「总览」以 DEMO 模式启动 → 观察后再切 LIVE

EOF
