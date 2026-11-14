#!/usr/bin/env bash
# 启动 NanoQuant (前台运行, Ctrl+C 退出; 生产建议用 systemd 或 nohup)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# 端口优先级：显式环境变量 > install 时写入的 data/.port > 默认 8899
PORT_FILE="$ROOT/data/.port"
INSTALLED_PORT="$(tr -dc '0-9' < "$PORT_FILE" 2>/dev/null || true)"
PORT="${NANOQUANT_PORT:-${INSTALLED_PORT:-8899}}"
VENV="$ROOT/.venv"

if [[ ! -x "$VENV/bin/python" ]]; then
  echo "未找到虚拟环境, 请先执行: bash scripts/install.sh"
  exit 1
fi

export NANOQUANT_HOME="$ROOT"
export NANOQUANT_PORT="$PORT"
mkdir -p "$ROOT/data" "$ROOT/logs"

cd "$ROOT/backend"

if [[ "${1:-}" == "--daemon" ]]; then
  PID_FILE="$ROOT/data/nanoquant.pid"
  if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    echo "已在运行 (PID $(cat "$PID_FILE"))"
    exit 0
  fi
  nohup "$VENV/bin/python" -m nanoquant >> "$ROOT/logs/stdout.log" 2>&1 &
  echo $! > "$PID_FILE"
  sleep 2
  if kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    echo "已后台启动 (PID $(cat "$PID_FILE")), 访问 http://127.0.0.1:$PORT"
  else
    echo "启动失败, 请查看 $ROOT/logs/stdout.log"
    exit 1
  fi
else
  exec "$VENV/bin/python" -m nanoquant
fi
