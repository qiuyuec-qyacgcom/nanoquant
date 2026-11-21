#!/usr/bin/env bash
# 停止 NanoQuant：有 systemd unit 且服务活跃时优先 systemctl，否则停止 nohup PID
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PID_FILE="$ROOT/data/nanoquant.pid"
SYSTEMD_UNIT="${NANOQUANT_SYSTEMD_UNIT:-nanoquant.service}"
SYSTEMD_UNIT_FILE="${NANOQUANT_SYSTEMD_UNIT_FILE:-/etc/systemd/system/$SYSTEMD_UNIT}"
FORCE_NOHUP=0

for arg in "$@"; do
  case "$arg" in
    --no-systemd) FORCE_NOHUP=1 ;;
    -h|--help)
      echo "用法: bash scripts/stop.sh [--no-systemd]"
      echo "  有 systemd unit 时自动 systemctl stop；--no-systemd 强制只停 PID/nohup"
      exit 0
      ;;
    *) echo "未知参数: $arg"; exit 2 ;;
  esac
done

run_systemctl() {
  if [[ "$(id -u)" -eq 0 ]]; then
    systemctl "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo systemctl "$@"
  else
    echo "需要 root 或 sudo 才能操作 systemd 服务" >&2
    exit 1
  fi
}

if [[ "$FORCE_NOHUP" -eq 0 ]] && command -v systemctl >/dev/null 2>&1 && [[ -f "$SYSTEMD_UNIT_FILE" ]]; then
  if systemctl is-active --quiet "$SYSTEMD_UNIT"; then
    echo "通过 systemd 停止: $SYSTEMD_UNIT"
    run_systemctl stop "$SYSTEMD_UNIT"
    echo "已停止"
    exit 0
  fi
  echo "systemd 服务未运行，继续检查 nohup 进程"
fi

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
  # 兜底: 按 Rust 二进制路径匹配（不匹配解释器）
  pkill -f "rust/nanoquant/target/release/nanoquant" 2>/dev/null && echo "已停止匹配进程" || echo "未发现运行中的 NanoQuant"
fi