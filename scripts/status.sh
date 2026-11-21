#!/usr/bin/env bash
# 查看 NanoQuant 运行状态：systemd 优先，nohup 兜底
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT_FILE="$ROOT/data/.port"
INSTALLED_PORT="$(tr -dc '0-9' < "$PORT_FILE" 2>/dev/null || true)"
PORT="${NANOQUANT_PORT:-${INSTALLED_PORT:-8899}}"
PID_FILE="$ROOT/data/nanoquant.pid"
SYSTEMD_UNIT="${NANOQUANT_SYSTEMD_UNIT:-nanoquant.service}"
SYSTEMD_UNIT_FILE="${NANOQUANT_SYSTEMD_UNIT_FILE:-/etc/systemd/system/$SYSTEMD_UNIT}"

echo "== 进程 =="
if command -v systemctl >/dev/null 2>&1 && [[ -f "$SYSTEMD_UNIT_FILE" ]]; then
  echo "systemd: $(systemctl is-active "$SYSTEMD_UNIT" 2>/dev/null || true)"
  systemctl --no-pager --lines=0 status "$SYSTEMD_UNIT" 2>/dev/null || true
elif [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  echo "运行中 (nohup PID $(cat "$PID_FILE"))"
elif pgrep -f "rust/nanoquant/target/release/nanoquant" >/dev/null 2>&1; then
  echo "运行中 (pgrep): $(pgrep -f 'rust/nanoquant/target/release/nanoquant' | tr '\n' ' ')"
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