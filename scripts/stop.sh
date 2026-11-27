#!/usr/bin/env bash
# 停止 NanoQuant：先停主服务，再停执行内核；有 systemd unit 时优先 systemctl。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PID_FILE="$ROOT/data/nanoquant.pid"
CORE_PID_FILE="$ROOT/data/exec-core.pid"
SYSTEMD_UNIT="${NANOQUANT_SYSTEMD_UNIT:-nanoquant.service}"
SYSTEMD_UNIT_FILE="${NANOQUANT_SYSTEMD_UNIT_FILE:-/etc/systemd/system/$SYSTEMD_UNIT}"
CORE_UNIT="${NANOQUANT_EXEC_CORE_UNIT:-nanoquant-exec-core.service}"
CORE_UNIT_FILE="${NANOQUANT_EXEC_CORE_UNIT_FILE:-/etc/systemd/system/$CORE_UNIT}"
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

stop_pid_file() {
  local file="$1" label="$2"
  if [[ -f "$file" ]]; then
    local pid
    pid="$(cat "$file")"
    if kill -0 "$pid" 2>/dev/null; then
      echo "停止 $label 进程 $pid"
      kill "$pid" 2>/dev/null || true
      for _ in $(seq 1 20); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.5
      done
      kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f "$file"
    return 0
  fi
  return 1
}

if [[ "$FORCE_NOHUP" -eq 0 ]] && command -v systemctl >/dev/null 2>&1 && [[ -f "$SYSTEMD_UNIT_FILE" ]]; then
  if systemctl is-active --quiet "$SYSTEMD_UNIT" 2>/dev/null; then
    echo "通过 systemd 停止主服务: $SYSTEMD_UNIT"
    run_systemctl stop "$SYSTEMD_UNIT"
  else
    echo "主服务 systemd 未运行，继续检查 nohup 进程"
  fi
  if [[ -f "$CORE_UNIT_FILE" ]]; then
    if systemctl is-active --quiet "$CORE_UNIT" 2>/dev/null; then
      echo "通过 systemd 停止执行内核: $CORE_UNIT"
      run_systemctl stop "$CORE_UNIT"
    fi
  fi
  echo "已停止"
  exit 0
fi

if stop_pid_file "$PID_FILE" "主服务"; then
  :
else
  # 兜底: 按 Rust 二进制路径匹配（不匹配解释器）
  pkill -f "rust/nanoquant/target/release/nanoquant" 2>/dev/null \
    && echo "已停止匹配的主服务进程" || echo "未发现运行中的 NanoQuant 主服务"
fi

if stop_pid_file "$CORE_PID_FILE" "执行内核"; then
  :
else
  pkill -f "$ROOT/exec-core" 2>/dev/null \
    && echo "已停止匹配的执行内核进程" || true
fi

echo "已停止"
