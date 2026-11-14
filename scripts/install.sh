#!/usr/bin/env bash
# NanoQuant 一键安装脚本（依赖安装这一步）
#
# 免 clone 的一条命令在线安装请用仓库根目录的引导脚本 install.sh：
#   if [ -f /usr/bin/curl ];then curl -sSO https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh;else wget -O install.sh https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/install.sh;fi;bash install.sh --password 'xxx'
# 引导脚本下载代码后会转交到本脚本继续安装。
#
#   sudo bash scripts/install.sh                 # 默认端口 8899
#   sudo bash scripts/install.sh --port 9000     # 指定端口（写进 data/.port，start.sh 沿用）
#   sudo bash scripts/install.sh --systemd       # 注册 systemd 自启，并等健康检查通过
#   sudo bash scripts/install.sh --no-frontend   # 跳过前端构建（已有 dist 或只跑后端）
#   sudo bash scripts/install.sh --mirror cn     # 用国内 pip 镜像（清华源）
#   sudo bash scripts/install.sh --password 'xxx' # 指定初始管理员密码
#   sudo bash scripts/install.sh --force         # 强制重装依赖（默认按 requirements 哈希跳过）
#   sudo bash scripts/install.sh --no-pyenv      # 不用 pyenv 兜底编译 Python（跳过自动编译）
#   sudo bash scripts/install.sh --no-start      # 装完不自动后台启动（默认装完即后台运行）
#
# 密码：不指定时由安装脚本随机生成、装完直接打印，登录后必须改密。
#
# 设计取舍：
# - 幂等：重复执行不会每次重装依赖/重建 venv，改完 requirements 再跑一次即可；
# - 失败可见：任何一步失败都以非 0 退出并说明下一步，绝不打印"安装完成"；
# - 尽量自动补齐：缺 Node 会自动装（apt → NodeSource 20 LTS），装不上才警告跳过；
#   Python 缺 sqlite3 或版本不够会自动补（libsqlite3-dev / apt 3.11+），
#   再不行用 pyenv 编译 3.11，还不行才中止。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${NANOQUANT_PORT:-8899}"
WITH_SYSTEMD=0
WITH_FRONTEND=1
FORCE=0
MIRROR="${PIP_INDEX_URL:-}"
ADMIN_PASSWORD="${NANOQUANT_ADMIN_PASSWORD:-}"
NO_PYENV=0
NO_START=0
PYENV_MIRROR=""

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
        cn|tsinghua) MIRROR="https://pypi.tuna.tsinghua.edu.cn/simple"; PYENV_MIRROR="https://mirrors.huaweicloud.com/python" ;;
        aliyun) MIRROR="https://mirrors.aliyun.com/pypi/simple"; PYENV_MIRROR="https://mirrors.huaweicloud.com/python" ;;
        *) MIRROR="${2:-}" ;;
      esac
      shift 2 ;;
    --no-pyenv) NO_PYENV=1; shift ;;
    --no-start) NO_START=1; shift ;;
    --password)
      ADMIN_PASSWORD="${2:-}"
      shift 2 ;;
    --force) FORCE=1; shift ;;
    -h|--help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
if [[ -n "$ADMIN_PASSWORD" ]]; then
  [[ ${#ADMIN_PASSWORD} -ge 8 ]] || die "--password 至少 8 位（这是唯一能进后台的凭据）"
  info "初始管理员密码: 使用你指定的值（不回显，装完会打印）"
else
  info "初始管理员密码: 由安装脚本随机生成，装完打印"
fi

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

# 与 backend/requirements.txt（uv 按 3.11 编译）和 CI 矩阵保持一致：
# 依赖元数据里 starlette 要求 >=3.11，所以 3.11 是最低可用版本（3.10 已被生态淘汰）。
# 另外必须能 import sqlite3：源码编译的 Python 若缺 libsqlite3-dev，
# _sqlite3 C 扩展会被静默跳过，版本看着够、装到一半才炸，这里提前揪出来。
MIN_MINOR=11
py_usable() {
  "$1" -c "import sys, sqlite3; sys.exit(0 if sys.version_info[:2] >= (3, $MIN_MINOR) else 1)" 2>/dev/null
}
# 只判版本（不查 sqlite3），用来区分「版本不够」和「版本够但缺 sqlite3」
py_version_ok() {
  "$1" -c "import sys; raise SystemExit(0 if sys.version_info[:2] >= (3, $MIN_MINOR) else 1)" 2>/dev/null
}
# 找到第一个「版本够且能 import sqlite3」的解释器，写入全局 PY（始终返回 0，避免 set -e 误杀）
scan_python() {
  PY=""
  local cand
  for cand in python3.13 python3.12 python3.11 python3; do
    command -v "$cand" >/dev/null 2>&1 || continue
    if py_usable "$cand"; then PY="$cand"; return 0; fi
  done
  return 0
}
# 找「版本够但缺 sqlite3」的解释器，用于精准报错
find_broken_sqlite() {
  local cand
  for cand in python3.13 python3.12 python3.11 python3; do
    command -v "$cand" >/dev/null 2>&1 || continue
    if py_version_ok "$cand" && ! py_usable "$cand"; then printf '%s\n' "$cand"; return 0; fi
  done
  return 1
}

# pyenv 兜底：apt 装不出 3.11+（更老的发行版源里只有 3.9/3.10）时，用 pyenv 编译一个完整版 3.11
PYENV_BIN=""
install_pyenv_python() {
  local pyenv_root="${PYENV_ROOT:-$HOME/.pyenv}"
  local ver="${NANOQUANT_PYENV_VERSION:-3.11.13}"
  PYENV_BIN=""
  # 已装过这个版本就直接复用
  if [[ -x "$pyenv_root/versions/$ver/bin/python" ]]; then
    PYENV_BIN="$pyenv_root/versions/$ver/bin/python"
    ok "复用 pyenv 已装好的 Python $ver"
    return 0
  fi
  # 1. pyenv 本体（git 优先，curl/wget 下载源码包兜底）
  if [[ ! -x "$pyenv_root/bin/pyenv" ]]; then
    if command -v git >/dev/null 2>&1; then
      info "安装 pyenv（git clone -> $pyenv_root）"
      git clone --depth 1 https://github.com/pyenv/pyenv.git "$pyenv_root" || return 1
    elif command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1; then
      info "安装 pyenv（下载源码包 -> $pyenv_root）"
      local tmpdir
      tmpdir="$(mktemp -d)" || return 1
      if command -v curl >/dev/null 2>&1; then
        curl -fsSL https://github.com/pyenv/pyenv/archive/refs/heads/master.tar.gz -o "$tmpdir/pyenv.tar.gz" || return 1
      else
        wget -qO "$tmpdir/pyenv.tar.gz" https://github.com/pyenv/pyenv/archive/refs/heads/master.tar.gz || return 1
      fi
      tar -xzf "$tmpdir/pyenv.tar.gz" -C "$tmpdir" || return 1
      mkdir -p "$(dirname "$pyenv_root")"
      [[ "$pyenv_root" == */.pyenv ]] && rm -rf "$pyenv_root"
      mv "$tmpdir/pyenv-master" "$pyenv_root" || return 1
      rm -rf "$tmpdir"
    else
      return 1
    fi
  fi
  [[ -x "$pyenv_root/bin/pyenv" ]] || return 1
  ok "pyenv 就绪"

  # 2. 编译 CPython 的构建依赖（apt 完整支持，其他包管理器尽力而为）
  if [[ "$PKG" == "apt" ]]; then
    info "安装编译 Python 的构建依赖"
    apt-get update -qq || true
    install_pkg build-essential libssl-dev zlib1g-dev libbz2-dev libreadline-dev \
      libsqlite3-dev curl libncursesw5-dev xz-utils tk-dev libxml2-dev libxmlsec1-dev \
      libffi-dev liblzma-dev || true
  else
    warn "非 apt 系统：pyenv 编译依赖可能不全（主要支持 Debian/Ubuntu）"
    install_pkg gcc make zlib-devel readline-devel sqlite-devel openssl-devel libffi-devel || true
  fi

  # 3. 编译（国内可走镜像加速下载 Python 源码）
  export PYENV_ROOT="$pyenv_root"
  [[ -n "$PYENV_MIRROR" ]] && export PYTHON_BUILD_MIRROR_URL="$PYENV_MIRROR"
  info "pyenv 编译 Python $ver（首次约 3-5 分钟）"
  "$pyenv_root/bin/pyenv" install -s "$ver" || return 1
  [[ -x "$pyenv_root/versions/$ver/bin/python" ]] || return 1
  PYENV_BIN="$pyenv_root/versions/$ver/bin/python"
  ok "pyenv 编译完成：$($PYENV_BIN --version 2>&1)"
  return 0
}

scan_python
if [[ -z "$PY" ]]; then
  BROKEN="$(find_broken_sqlite || true)"
  if [[ -n "$BROKEN" ]]; then
    warn "检测到 $BROKEN 版本满足要求，但缺少 sqlite3 模块（_sqlite3 C 扩展）"
    info "尝试自动修复：装 libsqlite3-dev，并尽量用 apt 装一个完整版 Python 绕开"
  else
    warn "未找到 Python 3.11+，尝试自动安装"
  fi
  if [[ "$PKG" == "apt" ]]; then
    apt-get update -qq || true
    # 缺 sqlite3 时补上开发库（重编才能成功）；再显式试装 3.11/3.12/3.13（Debian 12 源里有 3.11）
    [[ -n "$BROKEN" ]] && install_pkg libsqlite3-dev || true
    install_pkg python3 python3-venv python3-pip || true
    install_pkg python3.11 || true
    install_pkg python3.12 || true
    install_pkg python3.13 || true
  else
    install_pkg python3 python3-pip || true
  fi
  scan_python
fi

if [[ -z "$PY" ]]; then
  # 兜底：apt 装不出 3.11+（更老的发行版源里只有 3.9/3.10）时，用 pyenv 编译一个完整版 3.11
  if [[ "$NO_PYENV" == "1" ]]; then
    die "未找到可用 Python 3.11+，且已用 --no-pyenv 跳过自动编译。请自行安装 3.11+ 后重试"
  fi
  warn "apt 装不出 Python 3.11+，改用 pyenv 编译"
  if install_pyenv_python && py_usable "$PYENV_BIN"; then
    PY="$PYENV_BIN"
  else
    warn "pyenv 自动编译未成功，下面给出手动指引"
  fi
fi

if [[ -z "$PY" ]]; then
  if [[ -n "$BROKEN" ]]; then
    die "apt 与 pyenv 都未能给出可用 Python（$BROKEN 缺 sqlite3）。
已装好 libsqlite3-dev，可手动重编一次补 sqlite3：
  cd <你的 Python 源码目录> && ./configure --enable-optimizations && make -j\"\$(nproc)\" && make altinstall
或检查网络后重试（pyenv 需访问 github / python.org）"
  fi
  die "apt 与 pyenv 都未能安装 Python 3.11+。请检查网络后重试，或手动装 deadsnakes / pyenv / 官方源码"
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
# 自动补齐 Node.js：前端要 18+，缺了或太旧就自动装（apt 优先，装不到走 NodeSource 20 LTS）
node_ready() {
  command -v node >/dev/null 2>&1 || return 1
  local maj
  maj="$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)"
  [[ "${maj:-0}" -ge 18 ]]
}
ensure_node() {
  node_ready && return 0
  info "未检测到 Node.js 18+，尝试自动安装"
  if [[ "$PKG" == "apt" ]]; then
    apt-get update -qq || true
    install_pkg nodejs npm || true
    if node_ready; then ok "Node.js: $(node -v)"; return 0; fi
    # apt 源里的 nodejs 偏老（Ubuntu 22.04 只有 12），换 NodeSource 装 20 LTS
    if command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1; then
      warn "apt 的 nodejs 版本过旧（$(node -v 2>/dev/null || echo 无)），改用 NodeSource 装 Node 20 LTS"
      install_pkg ca-certificates gnupg || true
      if command -v curl >/dev/null 2>&1; then
        curl -fsSL https://deb.nodesource.com/setup_20.x | bash - 2>&1 | sed 's/^/  /' || true
      else
        wget -qO- https://deb.nodesource.com/setup_20.x | bash - 2>&1 | sed 's/^/  /' || true
      fi
      install_pkg nodejs || true
      node_ready && { ok "Node.js: $(node -v)"; return 0; }
    fi
    return 1
  fi
  install_pkg nodejs npm || true
  node_ready && { ok "Node.js: $(node -v)"; return 0; }
  return 1
}

if [[ "$WITH_FRONTEND" == "1" ]]; then
  if [[ -f "$ROOT/frontend/dist/index.html" ]]; then
    info "已存在前端产物，跳过构建（要重建请先删 frontend/dist）"
  elif ensure_node; then
    info "构建前端（npm ci + build）"
    if [[ -f "$ROOT/frontend/package-lock.json" ]]; then
      ( cd "$ROOT/frontend" && npm ci --no-audit --no-fund && npm run build ) || die "前端构建失败"
    else
      ( cd "$ROOT/frontend" && npm install --no-audit --no-fund && npm run build ) || die "前端构建失败"
    fi
    ok "前端构建完成（frontend/dist）"
  else
    warn "未能自动安装 Node.js 18+，跳过前端构建"
    warn "面板打不开但 API 正常；手动装好 Node 18+ 后执行: cd frontend && npm ci && npm run build"
  fi
fi

# ---------------------------------------------------------------- 运行目录
mkdir -p "$ROOT/data" "$ROOT/logs" "$ROOT/data/backups"
chmod 700 "$ROOT/data" 2>/dev/null || true
# 端口持久化：--port 要真的被 start.sh 沿用，否则只是个"提示里好看"的参数
printf '%s\n' "$PORT" > "$ROOT/data/.port" 2>/dev/null || true
# 初始密码：安装阶段还没有数据库，只能留成文件让首次启动时读取并哈希入库，
# 启动成功后该文件会被删掉（明文不留在盘上）。
# 未指定 --password 时，脚本自己生成随机密码并留一个空标记文件，
# 让首次启动知道「这是随机生成的，登录后仍需强制改密」。
PASSWORD_GENERATED=0
if [[ -n "$ADMIN_PASSWORD" ]]; then
  GEN_PASSWORD="$ADMIN_PASSWORD"
else
  GEN_PASSWORD="$("$PY" -c "import secrets; a='abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'; print(''.join(secrets.choice(a) for _ in range(16)))")"
  PASSWORD_GENERATED=1
fi
umask 077
printf '%s' "$GEN_PASSWORD" > "$ROOT/data/.admin_password"
chmod 600 "$ROOT/data/.admin_password" 2>/dev/null || true
if [[ "$PASSWORD_GENERATED" == "1" ]]; then
  : > "$ROOT/data/.admin_password_generated"
  chmod 600 "$ROOT/data/.admin_password_generated" 2>/dev/null || true
fi
ok "已暂存初始密码（首次启动写入数据库后自动删除）"
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

# ---------------------------------------------------------------- 自动后台启动（非 systemd）
# 没走 systemd 时，装完直接后台拉起服务（等价 start.sh --daemon），省去手动敲启动命令。
# 首次启动会顺带消费 data/.admin_password（哈希入库并删除），这里启动正好闭环。
# --no-start 可跳过（只想装好环境、稍后自己启动）。
if [[ "$WITH_SYSTEMD" != "1" && "$NO_START" != "1" ]]; then
  info "后台启动服务（等价 start.sh --daemon）"
  if bash "$ROOT/scripts/start.sh" --daemon; then
    info "等待健康检查（最多 30s）"
    HEALTHY=0
    for _ in $(seq 1 30); do
      if curl -fsS "http://127.0.0.1:$PORT/api/v1/health" >/dev/null 2>&1; then HEALTHY=1; break; fi
      sleep 1
    done
    if [[ "$HEALTHY" == "1" ]]; then
      ok "服务已在后台运行（健康检查通过）"
    else
      warn "已后台启动但健康检查未通过，日志见 $ROOT/logs/stdout.log"
    fi
  else
    warn "自动后台启动失败，请手动运行: bash scripts/start.sh --daemon"
  fi
fi

# hostname -I 不是所有发行版都支持（BusyBox / macOS 都没有），
# set -o pipefail 下失败会直接把脚本整段带走，所以这里必须兜住
IP="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
IP="${IP:-127.0.0.1}"

if [[ "$PASSWORD_GENERATED" == "1" ]]; then
  PASS_LINE="初始密码 : $GEN_PASSWORD"
  PASS_HINT="该密码由安装脚本随机生成，仅本次显示；登录后必须先改密。"
else
  PASS_LINE="初始密码 : $GEN_PASSWORD"
  PASS_HINT="登录后建议改成新的强口令（首次登录不强制）。"
fi

# 结尾「启动」一行按实际状态展示，避免误导
if [[ "$WITH_SYSTEMD" == "1" ]]; then
  START_LINE="启动     : systemd 已接管（开机自启；手动重启: systemctl restart nanoquant）"
elif [[ "$NO_START" == "1" ]]; then
  START_LINE="启动     : bash scripts/start.sh --daemon   （本次按 --no-start 未自动启动）"
else
  START_LINE="启动     : 已自动后台运行；机器重启后请 bash scripts/start.sh --daemon"
fi

cat <<EOF

$(printf "${C_OK}========================================${C_RESET}")
 安装完成
$(printf "${C_OK}========================================${C_RESET}")

  访问地址 : http://$IP:$PORT
  默认账号 : admin
  $PASS_LINE

  $(printf "${C_WARN}$PASS_HINT${C_RESET}")
  改密前后台除登录/改密/健康检查外的接口一律 403。

  $START_LINE
  停止     : bash scripts/stop.sh
  查看状态 : bash scripts/status.sh
  健康检查 : curl http://127.0.0.1:$PORT/api/v1/health

  数据目录 : $ROOT/data     (SQLite / 备份 / 缓存)
  日志目录 : $ROOT/logs
  参数配置 : 全部在面板里，存于数据库，改完即时生效

  首次使用建议顺序:
   1. 用初始密码登录，立即改成自己的强口令（随机密码首次登录会被强制）
   2. 「参数配置 → 数据源」填币安 API Key（只开合约交易，关闭提币）
   3. 「参数配置 → 系统 → 告警」配 Webhook / Telegram，否则出事只能在页面里看
   4. 「AI 模型」接入 DeepSeek / Qwen / GPT 等 OpenAI 兼容模型并点「诊断」
   5. 「标的池」添加合约 → 「总览」以 DEMO 模式启动 → 观察后再切 LIVE

EOF
