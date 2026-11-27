#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/rust/nanoquant/target/release/nanoquant"
CHECK=0
NO_BACKUP=0
NO_FRONTEND=0
WITH_EXEC_CORE=0
NO_EXEC_CORE=0
FORCE=0

usage() {
  cat <<'EOF'
用法: bash scripts/update.sh [选项]

选项:
  --check          只预览远端版本与工作树状态，不修改文件
  --no-backup      跳过更新前备份
  --no-frontend    只更新后端；要求已有 frontend/dist
  --with-exec-core 强制重编并重装执行内核（默认已安装则自动重建）
  --no-exec-core   跳过执行内核；仅更新 Web 主服务
  --force          已是最新也强制重建前端并重启
  -h, --help       显示帮助

更新前要求工作树干净；data/、.master_key 与数据库不会被覆盖。
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --check) CHECK=1; shift ;;
    --no-backup) NO_BACKUP=1; shift ;;
    --no-frontend) NO_FRONTEND=1; shift ;;
    --with-exec-core) WITH_EXEC_CORE=1; shift ;;
    --no-exec-core) NO_EXEC_CORE=1; shift ;;
    --force) FORCE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; echo "未知参数: $1" >&2; exit 1 ;;
  esac
done

if [ "$WITH_EXEC_CORE" -eq 1 ] && [ "$NO_EXEC_CORE" -eq 1 ]; then
  usage >&2
  echo "--with-exec-core 与 --no-exec-core 不能同时使用" >&2
  exit 1
fi

cd "$ROOT"
command -v git >/dev/null 2>&1 || { echo "缺少 git" >&2; exit 1; }

if [ "$CHECK" -eq 1 ]; then
  echo "工作树:"
  git status --short || true
  local_head="$(git rev-parse HEAD)"
  remote_head="$(git ls-remote origin HEAD 2>/dev/null | awk 'NR==1 {print $1}' || true)"
  echo "本地 HEAD: $local_head"
  if [ -n "$remote_head" ]; then
    echo "远端 HEAD: $remote_head"
    if [ "$local_head" = "$remote_head" ]; then
      echo "状态: 已是最新"
    else
      echo "状态: 有新提交（实际更新请运行 sudo bash scripts/update.sh）"
    fi
  else
    echo "状态: 无法读取远端（网络/凭据问题）"
  fi
  exit 0
fi

if ! git diff --quiet || ! git diff --cached --quiet || [ -n "$(git status --porcelain)" ]; then
  echo "工作树存在未提交改动，已中止；请先提交或 stash" >&2
  exit 1
fi

echo "[INFO] 拉取代码"
git pull --ff-only

if [ "$NO_BACKUP" -eq 0 ] && [ -x "$BIN" ]; then
  echo "[INFO] 更新前备份"
  "$BIN" ops backup-create --name "pre-update-$(date +%Y%m%d%H%M%S)" 2>/dev/null \
    || echo "[WARN] 备份失败，继续更新" >&2
fi

[ -f "$ROOT/scripts/install.sh" ] || { echo "缺少 scripts/install.sh" >&2; exit 1; }
args=(--force-frontend)
if [ "$NO_FRONTEND" -eq 1 ]; then
  args=(--no-frontend)
fi
if [ "$NO_EXEC_CORE" -eq 1 ]; then
  args+=(--no-exec-core)
elif [ "$WITH_EXEC_CORE" -eq 1 ]; then
  args+=(--with-exec-core)
fi
echo "[INFO] 构建并重启（install.sh ${args[*]}）"
bash "$ROOT/scripts/install.sh" "${args[@]}"

echo "[OK] 更新完成"