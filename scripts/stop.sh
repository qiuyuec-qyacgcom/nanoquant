#!/usr/bin/env bash
# 停止 NanoQuant
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PID_FILE="$ROOT/data/nanoquant.pid"

if [[ -f "$PID_FILE" ]]; then
  PID="$(cat "$PID_FILE")"
  if kill -0 "$PID" 2>/dev/null; then
    echo "停止进程 $PID"
    kill "$PID"
    for _ in $(seq 1 20); do
      kill -0 "$PID" 2>/dev/null || break
      sleep 0.5
    done
    kill -9 "$PID" 2>/dev/null || true
  fi
  rm -f "$PID_FILE"
  echo "已停止"
else
  # 兜底: 按命令行匹配
  pkill -f "python -m nanoquant" 2>/dev/null && echo "已停止匹配进程" || echo "未发现运行中的 NanoQuant"
fi
