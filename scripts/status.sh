#!/usr/bin/env bash
# 查看 NanoQuant 运行状态：主服务、执行内核、接口、心跳与磁盘。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT_FILE="$ROOT/data/.port"
INSTALLED_PORT=""
if [[ -f "$PORT_FILE" ]]; then
  INSTALLED_PORT="$(tr -dc '0-9' < "$PORT_FILE" 2>/dev/null || true)"
fi
PORT="${NANOQUANT_PORT:-${INSTALLED_PORT:-8899}}"
PID_FILE="$ROOT/data/nanoquant.pid"
CORE_PID_FILE="$ROOT/data/exec-core.pid"
CORE_ENV_FILE="${NANOQUANT_EXEC_ENV_FILE:-$ROOT/data/exec.env}"
SYSTEMD_UNIT="${NANOQUANT_SYSTEMD_UNIT:-nanoquant.service}"
SYSTEMD_UNIT_FILE="${NANOQUANT_SYSTEMD_UNIT_FILE:-/etc/systemd/system/$SYSTEMD_UNIT}"
CORE_UNIT="${NANOQUANT_EXEC_CORE_UNIT:-nanoquant-exec-core.service}"
CORE_UNIT_FILE="${NANOQUANT_EXEC_CORE_UNIT_FILE:-/etc/systemd/system/$CORE_UNIT}"

echo "== 主服务 =="
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
echo "== 执行内核 exec-core =="
if command -v systemctl >/dev/null 2>&1 && [[ -f "$CORE_UNIT_FILE" ]]; then
  echo "systemd: $(systemctl is-active "$CORE_UNIT" 2>/dev/null || true)"
  systemctl --no-pager --lines=0 status "$CORE_UNIT" 2>/dev/null || true
  if systemctl is-active --quiet "$CORE_UNIT" 2>/dev/null; then
    echo "端点: unix:/run/nanoquant/exec.sock"
  fi
elif [[ -f "$CORE_PID_FILE" ]] && kill -0 "$(cat "$CORE_PID_FILE")" 2>/dev/null; then
  echo "运行中 (nohup PID $(cat "$CORE_PID_FILE"))"
  echo "端点: tcp:${EXEC_TCP:-127.0.0.1:9101}"
elif [[ -x "$ROOT/exec-core" ]]; then
  echo "已安装但未运行"
  echo "端点: tcp:${EXEC_TCP:-127.0.0.1:9101}"
else
  echo "未安装"
fi
if [[ -f "$CORE_ENV_FILE" ]]; then
  echo "Secret: 已配置 ($CORE_ENV_FILE)"
else
  echo "Secret: 未配置（交易引擎不会启动）"
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
