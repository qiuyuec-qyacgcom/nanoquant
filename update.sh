#!/usr/bin/env bash
# NanoQuant 一行在线更新（已部署机器）
#
# 用法（任意目录执行）：
#   bash <(curl -sSL https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/update.sh)
#   bash <(curl -sSL .../update.sh) --check
#   bash <(curl -sSL .../update.sh) --no-frontend
#   NANOQUANT_HOME=/opt/nanoquant bash <(curl -sSL .../update.sh)
#
# 它做一件事：找到已部署的仓库 → 从远端取**最新的** scripts/update.sh → 执行，
# 这样你永远用的是最新更新逻辑，而不是机器上可能过期的那份。
set -euo pipefail

C_RESET='\033[0m'; C_OK='\033[32m'; C_WARN='\033[33m'; C_ERR='\033[31m'; C_INFO='\033[36m'
info() { printf "${C_INFO}[信息]${C_RESET} %s\n" "$*"; }
ok()   { printf "${C_OK}[完成]${C_RESET} %s\n" "$*"; }
warn() { printf "${C_WARN}[注意]${C_RESET} %s\n" "$*"; }
die()  { printf "${C_ERR}[失败]${C_RESET} %s\n" "$*" >&2; exit 1; }

# ---------------------------------------------------------------- 1. 定位已部署仓库
# 优先级：显式环境变量 > 当前目录 > systemd unit 里的 WorkingDirectory（最可靠）> 常见路径 > 浅层搜索
ROOT=""
is_repo() { [[ -d "$1/.git" && -d "$1/backend" ]]; }

if [[ -n "${NANOQUANT_HOME:-}" ]]; then
  ROOT="$NANOQUANT_HOME"
elif is_repo "$(pwd)"; then
  ROOT="$(pwd)"
else
  # systemd unit 里写了 WorkingDirectory=/实际/路径，直接读出来最准
  for _unit in /etc/systemd/system/nanoquant.service /lib/systemd/system/nanoquant.service \
               /usr/lib/systemd/system/nanoquant.service; do
    [[ -f "$_unit" ]] || continue
    _wd="$(sed -n 's/^[[:space:]]*WorkingDirectory[[:space:]]*=[[:space:]]*//p' "$_unit" | head -1)"
    if [[ -n "$_wd" ]] && is_repo "$_wd"; then ROOT="$_wd"; break; fi
  done
  # 常见安装路径
  if [[ -z "$ROOT" ]]; then
    for _p in "$HOME/nanoquant" /root/nanoquant /opt/nanoquant /srv/nanoquant \
              /data/nanoquant /var/www/nanoquant /usr/local/nanoquant; do
      if is_repo "$_p"; then ROOT="$_p"; break; fi
    done
  fi
  # 最后兜底：限定深度搜一下（别扫整个文件系统）
  if [[ -z "$ROOT" ]]; then
    for _base in "$HOME" /root /opt /srv /data /var/www /usr/local; do
      [[ -d "$_base" ]] || continue
      while IFS= read -r _d; do
        if is_repo "$_d"; then ROOT="$_d"; break 2; fi
      done < <(find "$_base" -maxdepth 3 -type d -name nanoquant 2>/dev/null)
    done
  fi
fi

if ! is_repo "$ROOT"; then
  echo
  echo "找不到已部署的 nanoquant 仓库。请先执行下面这条命令找到路径：" >&2
  echo "  find / -maxdepth 5 -type d -name nanoquant 2>/dev/null" >&2
  echo >&2
  echo "找到后（假设是 /xxx/nanoquant），再执行：" >&2
  echo "  NANOQUANT_HOME=/xxx/nanoquant bash <(curl -sSL https://raw.githubusercontent.com/qiuyuec-qyacgcom/nanoquant/main/update.sh)" >&2
  exit 1
fi

cd "$ROOT"
info "已定位仓库: $ROOT"

# ---------------------------------------------------------------- 2. 取远端最新 update.sh
command -v git >/dev/null 2>&1 || die "未找到 git"

BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo main)"
git remote get-url origin >/dev/null 2>&1 || die "仓库未配置 origin 远端"

info "从 origin/$BRANCH 读取更新脚本（保证用最新版）"
git fetch --quiet origin "$BRANCH" 2>/dev/null || die "git fetch 失败：检查网络/凭据"

REF="origin/$BRANCH"
git rev-parse --verify "$REF" >/dev/null 2>&1 || REF="origin/main"
git rev-parse --verify "$REF" >/dev/null 2>&1 || die "远端分支 $REF 不存在"

UPDATE_SCRIPT="$(mktemp /tmp/nanoquant-update.XXXXXX.sh)"
trap 'rm -f "$UPDATE_SCRIPT"' EXIT

git show "$REF:scripts/update.sh" > "$UPDATE_SCRIPT" 2>/dev/null \
  || die "远端 scripts/update.sh 不存在（仓库版本过旧？请手动 git pull 一次）"
chmod +x "$UPDATE_SCRIPT"
ok "已取得远端更新脚本（$REF）"

# ---------------------------------------------------------------- 3. 执行（透传所有参数）
info "开始更新（参数: ${*:-无}）"
# 内部 scripts/update.sh 用 BASH_SOURCE 定位仓库；这里脚本在 /tmp，必须显式告诉它真实根目录
export NANOQUANT_ROOT="$ROOT"
exec bash "$UPDATE_SCRIPT" "$@"
