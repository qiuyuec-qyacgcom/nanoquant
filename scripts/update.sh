#!/usr/bin/env bash
# NanoQuant 更新脚本（已部署机器上升级到最新代码）
#
# 为什么需要单独一个脚本，而不是直接重跑 install.sh：
#   1. install.sh 发现 `frontend/dist` 已存在就**跳过前端构建** —— 更新后你会继续看旧前端；
#   2. install.sh 不处理"本地有改动 / 落后多少提交"，直接 pull 可能冲突；
#   3. 更新前应该先备份 data/（数据库、密钥、备份），而不是指望手动记得。
#
# 用法（在仓库根目录，root 权限）：
#   sudo bash scripts/update.sh                # 拉代码 + 装依赖 + 重建前端 + 重启
#   sudo bash scripts/update.sh --check        # 只看会更新什么，不做任何改动
#   sudo bash scripts/update.sh --no-backup    # 跳过更新前备份
#   sudo bash scripts/update.sh --no-frontend  # 只更新后端（已有可用 dist）
#   sudo bash scripts/update.sh --with-sidecar # 顺带重编 Rust sidecar（之前装过才需要）
#   sudo bash scripts/update.sh --force        # 强制重装依赖并重建前端
#
# 安全边界：
#   - **不覆盖本地改动**：有未提交修改就中止，并告诉你该怎么处理；
#   - **不覆盖 data/**：数据、主密钥、备份都不动；
#   - 依赖按 requirements.txt 哈希跳过（与 install.sh 同口径）；
#   - 前端**无论如何都会重建**（除非 --no-frontend）——这是与 install.sh 的关键差异；
#   - 重启后做健康检查，失败会打印日志位置而不是假装成功。
set -euo pipefail

# NANOQUANT_ROOT 由根目录 update.sh 引导脚本传入（内部脚本在 /tmp 时 BASH_SOURCE 不可靠）
ROOT="${NANOQUANT_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
PORT=""
CHECK_ONLY=0
DO_BACKUP=1
DO_FRONTEND=1
DO_SIDECAR=0
FORCE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --check) CHECK_ONLY=1; shift ;;
    --no-backup) DO_BACKUP=0; shift ;;
    --no-frontend) DO_FRONTEND=0; shift ;;
    --with-sidecar) DO_SIDECAR=1; shift ;;
    --force) FORCE=1; shift ;;
    -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1（用 --help 查看用法）"; exit 1 ;;
  esac
done

C_RESET='\033[0m'; C_OK='\033[32m'; C_WARN='\033[33m'; C_ERR='\033[31m'; C_INFO='\033[36m'
info() { printf "${C_INFO}[信息]${C_RESET} %s\n" "$*"; }
ok()   { printf "${C_OK}[完成]${C_RESET} %s\n" "$*"; }
warn() { printf "${C_WARN}[注意]${C_RESET} %s\n" "$*"; }
die()  { printf "${C_ERR}[失败]${C_RESET} %s\n" "$*" >&2; exit 1; }

VENV="$ROOT/.venv"
[[ -d "$ROOT/.git" ]] || die "$ROOT 不是 git 仓库：无法用本脚本更新（手工替换代码后重启亦可）"

info "项目目录: $ROOT"

# ---------------------------------------------------------------- 1. 预检
cd "$ROOT"

if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
  die "当前不是有效的 git 工作区"
fi

if ! git remote get-url origin >/dev/null 2>&1; then
  die "未配置 origin 远端，无法拉取更新"
fi

# 未提交改动：绝不静默覆盖。用户可能改过参数默认值或本地补丁。
DIRTY="$(git status --porcelain --untracked-files=no || true)"
if [[ -n "$DIRTY" ]]; then
  warn "检测到未提交的代码改动："
  printf '%s\n' "$DIRTY" | sed 's/^/  /'
  if [[ "$CHECK_ONLY" != "1" ]]; then
    die "已中止（避免覆盖你的修改）。请先 git stash / 提交 / 丢弃后再更新：
  git stash push -u -m 'update-$(date +%Y%m%d-%H%M%S)'
  sudo bash scripts/update.sh
  git stash pop   # 需要的话"
  fi
  warn "--check：继续预览（不会改动任何文件）"
fi

info "拉取远端信息"
git fetch --prune origin 2>/dev/null || die "git fetch 失败：检查网络或 git 凭据"

CUR="$(git rev-parse --short HEAD)"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
UPSTREAM="origin/$BRANCH"
git rev-parse --verify "$UPSTREAM" >/dev/null 2>&1 || die "找不到上游分支 $UPSTREAM"

TARGET="$(git rev-parse --short "$UPSTREAM")"
BEHIND="$(git rev-list --count "HEAD..$UPSTREAM")"
BEHIND_CHANGES="$(git diff --name-only "HEAD..$UPSTREAM" | wc -l | tr -d ' ')"

echo
info "当前版本: $CUR"
info "远端版本: $TARGET（$UPSTREAM）"
if [[ "$BEHIND" == "0" ]]; then
  ok "已经是最新版本，无需更新"
  [[ "$CHECK_ONLY" == "1" ]] && exit 0
  # 仍然允许 --force 走一遍重建（前端/依赖可能被手动删过）
  if [[ "$FORCE" != "1" ]]; then
    exit 0
  fi
  warn "--force：跳过拉取，强制重建依赖与前端"
else
  info "落后 $BEHIND 个提交，涉及 $BEHIND_CHANGES 个文件"
  git log --oneline "HEAD..$UPSTREAM" | head -20 | sed 's/^/  /'
  if [[ "$BEHIND_CHANGES" != "0" ]]; then
    echo "  变更文件（最多 30 个）:"
    git diff --name-status "HEAD..$UPSTREAM" | head -30 | sed 's/^/    /'
  fi
fi
echo

if [[ "$CHECK_ONLY" == "1" ]]; then
  ok "--check：仅预览，未做任何改动"
  exit 0
fi

# 真正开始改动前，才要求这台机器已经装过（有 venv）
[[ -x "$VENV/bin/python" ]] || die "未找到 $VENV：这台机器还没安装过，请先跑 bash scripts/install.sh"

if [[ "$BEHIND" != "0" ]]; then
  info "拉取代码（--ff-only，失败即说明有分叉，不会自动 merge）"
  git merge --ff-only "$UPSTREAM" || die "无法快进合并（本地与远端有分叉）。
请人工处理：git status / git log --oneline --graph HEAD...$UPSTREAM"
  ok "代码已更新到 $TARGET"
fi

# ---------------------------------------------------------------- 2. 备份
if [[ "$DO_BACKUP" == "1" ]]; then
  info "更新前备份（数据库 + 配置 + 主密钥）"
  if ( cd "$ROOT/backend" && "$VENV/bin/python" - <<'PY'
import sys
try:
    from nanoquant.services.backup import create_backup
    out = create_backup(name="pre-update", created_by="update-script")
    print(out.get("path") or out.get("name") or "ok")
except Exception as exc:  # noqa: BLE001
    print(f"BACKUP_FAILED: {exc}", file=sys.stderr)
    raise SystemExit(1)
PY
  ); then
    ok "备份完成（见 data/backups/）"
  else
    warn "备份失败：可加 --no-backup 跳过，但要自行确认 data/ 有可用副本"
    die "已中止（避免无备份升级）"
  fi
fi

# ---------------------------------------------------------------- 3. 依赖
REQ="$ROOT/backend/requirements.txt"
HASH_FILE="$VENV/.requirements.sha256"
NEW_HASH="$(sha256sum "$REQ" | awk '{print $1}')"
if [[ "$FORCE" == "0" && -f "$HASH_FILE" && "$(cat "$HASH_FILE" 2>/dev/null)" == "$NEW_HASH" ]]; then
  info "后端依赖未变，跳过安装（--force 可强制）"
else
  info "安装/更新后端依赖"
  "$VENV/bin/python" -m pip install -q --upgrade pip >/dev/null 2>&1 || true
  "$VENV/bin/python" -m pip install -q -r "$REQ" || die "依赖安装失败：可检查网络/镜像后重试"
  printf '%s\n' "$NEW_HASH" > "$HASH_FILE"
  ok "后端依赖已更新"
fi

( cd "$ROOT/backend" && "$VENV/bin/python" -c "import nanoquant.main" ) \
  || die "依赖装完但 import 失败，请把上面的报错贴出来"
ok "后端可导入"

# 数据库结构自愈：老库缺列由 init_db 自动补齐（不删数据）
info "检查数据库结构（缺失的列/表会自动补，不删数据）"
( cd "$ROOT/backend" && "$VENV/bin/python" -c "
from nanoquant.db import init_db
init_db()
print('schema ok')
" ) || die "数据库初始化失败，请检查 data/ 权限或磁盘空间"
ok "数据库结构就绪"

# ---------------------------------------------------------------- 4. 前端（关键：强制重建）
if [[ "$DO_FRONTEND" == "1" ]]; then
  info "重建前端（更新后必须重建，否则浏览器仍跑旧界面）"
  command -v node >/dev/null 2>&1 || die "未找到 node：请先装 Node 18+，或用 --no-frontend 只更新后端"
  # 先清掉旧产物：既避免残留 hash 文件，也防止构建失败时"看起来还是好的"
  rm -rf "$ROOT/frontend/dist"
  if [[ -f "$ROOT/frontend/package-lock.json" ]]; then
    ( cd "$ROOT/frontend" && npm ci --no-audit --no-fund && npm run build ) || die "前端构建失败"
  else
    ( cd "$ROOT/frontend" && npm install --no-audit --no-fund && npm run build ) || die "前端构建失败"
  fi
  [[ -f "$ROOT/frontend/dist/index.html" ]] || die "前端构建完成但没有 dist/index.html"
  ok "前端已重建"
else
  warn "按 --no-frontend 跳过前端构建（界面可能仍是旧版）"
fi

# ---------------------------------------------------------------- 5. Rust sidecar（可选）
if [[ "$DO_SIDECAR" == "1" ]]; then
  if [[ ! -x "$ROOT/exec-service" ]]; then
    warn "未发现已安装的 exec-service：跳过（需要时用 scripts/setup_sidecar.sh --from-settings）"
  elif ! command -v cargo >/dev/null 2>&1; then
    warn "未找到 cargo：跳过 sidecar 重编（已安装的旧二进制继续可用）"
  else
    info "重编 Rust sidecar"
    if ( cd "$ROOT/rust/exec-service" && cargo build --release ) 2>&1 | tail -5 | sed 's/^/  /'; then
      install -m 0755 "$ROOT/rust/exec-service/target/release/exec-service" "$ROOT/exec-service"
      ok "sidecar 二进制已更新"
    else
      warn "sidecar 编译失败：保留旧二进制，主服务不受影响"
    fi
  fi
fi

# ---------------------------------------------------------------- 6. 重启
SYSTEMD_UNIT=""
if command -v systemctl >/dev/null 2>&1 && systemctl list-unit-files 2>/dev/null | grep -q '^nanoquant\.service'; then
  SYSTEMD_UNIT="nanoquant"
fi

if [[ -n "$SYSTEMD_UNIT" ]]; then
  info "重启 systemd 服务"
  systemctl restart nanoquant || die "重启失败：journalctl -u nanoquant -n 50 --no-pager"
else
  info "重启后台进程（非 systemd）"
  bash "$ROOT/scripts/stop.sh" >/dev/null 2>&1 || true
  bash "$ROOT/scripts/start.sh" --daemon || die "后台启动失败：见 logs/stdout.log"
fi

# sidecar 也随之重启（存在才动）
if command -v systemctl >/dev/null 2>&1 && systemctl list-unit-files 2>/dev/null | grep -q '^nanoquant-exec\.service'; then
  systemctl restart nanoquant-exec 2>/dev/null || warn "nanoquant-exec 重启失败：journalctl -u nanoquant-exec -n 30"
fi

# ---------------------------------------------------------------- 7. 健康检查
PORT_FILE="$ROOT/data/.port"
INSTALLED_PORT="$(tr -dc '0-9' < "$PORT_FILE" 2>/dev/null || true)"
PORT="${NANOQUANT_PORT:-${INSTALLED_PORT:-8899}}"

info "健康检查（最多 30s，端口 $PORT）"
HEALTHY=0
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:$PORT/api/v1/health" >/dev/null 2>&1; then HEALTHY=1; break; fi
  sleep 1
done

echo
if [[ "$HEALTHY" == "1" ]]; then
  ok "更新完成：$CUR → $(git rev-parse --short HEAD)"
  echo "  访问地址 : http://<你的IP>:$PORT"
  echo "  查看状态 : bash scripts/status.sh"
else
  warn "代码与依赖已更新，但健康检查未通过"
  echo "  日志 : $ROOT/logs/nanoquant.log"
  echo "         journalctl -u nanoquant -n 50 --no-pager"
  echo "  回滚 : cd $ROOT && git reset --hard $CUR && sudo bash scripts/update.sh --force"
  exit 1
fi
