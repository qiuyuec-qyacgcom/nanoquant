#!/usr/bin/env bash
# 查看 NanoQuant 运行状态
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${NANOQUANT_PORT:-8899}"
PID_FILE="$ROOT/data/nanoquant.pid"

echo "== 进程 =="
if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  echo "运行中 (PID $(cat "$PID_FILE"))"
elif pgrep -f "python -m nanoquant" >/dev/null 2>&1; then
  echo "运行中 (pgrep): $(pgrep -f 'python -m nanoquant' | tr '\n' ' ')"
else
  echo "未运行"
fi

echo
echo "== 接口 =="
if command -v curl >/dev/null 2>&1; then
  curl -s -m 5 "http://127.0.0.1:$PORT/api/v1/health" || echo "健康检查失败"
fi

echo
echo "== 心跳 =="
if [[ -f "$ROOT/data/engine.heartbeat" ]]; then
  HEART="$(head -1 "$ROOT/data/engine.heartbeat")"
  echo "最近心跳时间戳: $HEART (当前 $(date +%s))"
else
  echo "无心跳文件"
fi

echo
echo "== 磁盘 =="
du -sh "$ROOT/data" "$ROOT/logs" 2>/dev/null || true
