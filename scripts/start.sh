#!/usr/bin/env bash
# 启动 NanoQuant：有 systemd unit 时优先 systemctl，否则前台/nohup 运行
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT_FILE="$ROOT/data/.port"
INSTALLED_PORT="$(tr -dc '0-9' < "$PORT_FILE" 2>/dev/null || true)"
PORT="${NANOQUANT_PORT:-${INSTALLED_PORT:-8899}}"
RUST_BIN="$ROOT/rust/nanoquant/target/release/nanoquant"
SYSTEMD_UNIT="${NANOQUANT_SYSTEMD_UNIT:-nanoquant.service}"
SYSTEMD_UNIT_FILE="${NANOQUANT_SYSTEMD_UNIT_FILE:-/etc/systemd/system/$SYSTEMD_UNIT}"

DAEMON=0
FORCE_NOHUP=0

usage() {
  echo "用法: bash scripts/start.sh [--daemon] [--no-systemd]"
  echo "  有 systemd unit 时自动 systemctl start；--no-systemd 强制 nohup/前台启动"
}

for arg in "$@"; do
  case "$arg" in
    --daemon) DAEMON=1 ;;
    --no-systemd) FORCE_NOHUP=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "未知参数: $arg"; usage; exit 2 ;;
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

has_systemd_unit() {
  [[ "$FORCE_NOHUP" -eq 0 ]] && command -v systemctl >/dev/null 2>&1 && [[ -f "$SYSTEMD_UNIT_FILE" ]]
}

export NANOQUANT_HOME="$ROOT"
export NANOQUANT_PORT="$PORT"
mkdir -p "$ROOT/data" "$ROOT/logs" "$ROOT/data/backups"

if has_systemd_unit; then
  if systemctl is-active --quiet "$SYSTEMD_UNIT"; then
    echo "已在运行（systemd: $SYSTEMD_UNIT）"
    exit 0
  fi
  echo "通过 systemd 启动: $SYSTEMD_UNIT"
  run_systemctl start "$SYSTEMD_UNIT"
  echo "已请求启动，查看状态: sudo systemctl status $SYSTEMD_UNIT --no-pager"
  exit 0
fi

if [[ ! -x "$RUST_BIN" ]]; then
  echo "未找到 Rust 二进制: $RUST_BIN"
  echo "请先执行: cargo build --release --manifest-path rust/nanoquant/Cargo.toml"
  exit 1
fi

if [[ "$DAEMON" -eq 1 ]]; then
  PID_FILE="$ROOT/data/nanoquant.pid"
  if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    echo "已在运行 (PID $(cat "$PID_FILE"))"
    exit 0
  fi
  nohup "$RUST_BIN" >> "$ROOT/logs/stdout.log" 2>&1 &
  echo $! > "$PID_FILE"
  sleep 2
  if kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    echo "已后台启动 Rust 内核 (PID $(cat "$PID_FILE")), 访问 http://127.0.0.1:$PORT"
  else
    echo "启动失败, 请查看 $ROOT/logs/stdout.log"
    exit 1
  fi
  exit 0
fi

exec "$RUST_BIN"