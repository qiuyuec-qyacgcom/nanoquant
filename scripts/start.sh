#!/usr/bin/env bash
# 启动 NanoQuant：有 systemd unit 时优先 systemctl，否则前台/nohup 运行。
# 默认执行内核 exec-core 会先于主服务启动；缺少 Secret 时主服务仍可启动，
# 但交易引擎不会启动。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT_FILE="$ROOT/data/.port"
INSTALLED_PORT=""
if [[ -f "$PORT_FILE" ]]; then
  INSTALLED_PORT="$(tr -dc '0-9' < "$PORT_FILE" 2>/dev/null || true)"
fi
PORT="${NANOQUANT_PORT:-${INSTALLED_PORT:-8899}}"
RUST_BIN="$ROOT/rust/nanoquant/target/release/nanoquant"
SYSTEMD_UNIT="${NANOQUANT_SYSTEMD_UNIT:-nanoquant.service}"
SYSTEMD_UNIT_FILE="${NANOQUANT_SYSTEMD_UNIT_FILE:-/etc/systemd/system/$SYSTEMD_UNIT}"
CORE_UNIT="${NANOQUANT_EXEC_CORE_UNIT:-nanoquant-exec-core.service}"
CORE_UNIT_FILE="${NANOQUANT_EXEC_CORE_UNIT_FILE:-/etc/systemd/system/$CORE_UNIT}"
CORE_BIN="$ROOT/exec-core"
CORE_PID_FILE="$ROOT/data/exec-core.pid"
CORE_ENV_FILE="${NANOQUANT_EXEC_ENV_FILE:-$ROOT/data/exec.env}"
EXTERNAL_EXEC="${EXEC_TCP:-}"
EXEC_ADDR="${EXEC_TCP:-127.0.0.1:9101}"

DAEMON=0
FORCE_NOHUP=0
CORE_STARTED=0

usage() {
  echo "用法: bash scripts/start.sh [--daemon] [--no-systemd]"
  echo "  有 systemd unit 时自动 systemctl start；--no-systemd 强制 nohup/前台启动"
  echo "  启动顺序: exec-core → 主服务；外部 EXEC_TCP 已设置时跳过本地 exec-core"
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

systemd_active() {
  systemctl is-active --quiet "$1" 2>/dev/null
}

has_systemd_unit() {
  [[ "$FORCE_NOHUP" -eq 0 ]] && command -v systemctl >/dev/null 2>&1 && [[ -f "$SYSTEMD_UNIT_FILE" ]]
}

read_core_secret() {
  local file="${NANOQUANT_EXEC_ENV_FILE:-$CORE_ENV_FILE}"
  if [[ -n "${EXEC_SECRET:-}" ]]; then
    printf '%s' "$EXEC_SECRET"
    return 0
  fi
  [[ -f "$file" ]] || return 1
  local value
  value="$(sed -n 's/^EXEC_SECRET=//p' "$file" | head -n 1)"
  [[ -n "$value" ]] || return 1
  printf '%s' "$value"
}

# 返回 0=本次新启动，1=已在运行，2=未启动（缺二进制/密钥/失败）
start_core_systemd() {
  if [[ ! -f "$CORE_UNIT_FILE" ]]; then
    echo "未安装 $CORE_UNIT；先运行 sudo bash scripts/setup_core.sh --from-settings" >&2
    return 2
  fi
  if systemd_active "$CORE_UNIT"; then
    echo "执行内核已在运行（$CORE_UNIT）"
    return 1
  fi
  run_systemctl start "$CORE_UNIT" >/dev/null 2>&1 || true
  sleep 1
  if systemd_active "$CORE_UNIT"; then
    echo "执行内核已启动（$CORE_UNIT）"
    return 0
  fi
  echo "执行内核未启动：检查 /etc/nanoquant/exec.env 与 journalctl -u $CORE_UNIT -n 30 --no-pager" >&2
  return 2
}

# 返回 0=本次新启动，1=已在运行，2=未启动（缺二进制/密钥/失败）
start_core_nohup() {
  if [[ ! -x "$CORE_BIN" ]]; then
    echo "未安装执行内核二进制：$CORE_BIN（先运行 bash scripts/setup_core.sh --no-systemd --from-settings）" >&2
    return 2
  fi
  if [[ -f "$CORE_PID_FILE" ]] && kill -0 "$(cat "$CORE_PID_FILE")" 2>/dev/null; then
    echo "执行内核已在运行（PID $(cat "$CORE_PID_FILE")，$EXEC_ADDR）"
    return 1
  fi
  local secret
  if ! secret="$(read_core_secret)"; then
    echo "执行内核缺少 EXEC_SECRET：先配置 API 凭证并运行 bash scripts/setup_core.sh --no-systemd --from-settings" >&2
    return 2
  fi
  mkdir -p "$ROOT/logs" "$ROOT/data"
  EXEC_SECRET="$secret" EXEC_TCP="$EXEC_ADDR" nohup "$CORE_BIN" >>"$ROOT/logs/exec-core.log" 2>&1 &
  local core_pid=$!
  echo "$core_pid" >"$CORE_PID_FILE"
  sleep 1
  if kill -0 "$core_pid" 2>/dev/null; then
    echo "执行内核已启动（PID $core_pid，$EXEC_ADDR）"
    return 0
  fi
  rm -f "$CORE_PID_FILE"
  echo "执行内核启动失败，查看 $ROOT/logs/exec-core.log" >&2
  return 2
}

export NANOQUANT_HOME="$ROOT"
export NANOQUANT_PORT="$PORT"
mkdir -p "$ROOT/data" "$ROOT/logs" "$ROOT/data/backups"

if has_systemd_unit; then
  if [[ -z "$EXTERNAL_EXEC" ]]; then
    set +e
    start_core_systemd
    core_rc=$?
    set -e
    [[ "$core_rc" -eq 0 ]] && CORE_STARTED=1
  else
    echo "使用外部执行内核：$EXTERNAL_EXEC"
  fi
  if systemd_active "$SYSTEMD_UNIT"; then
    if [[ "$CORE_STARTED" -eq 1 ]]; then
      echo "执行内核刚启动，重启主服务以完成握手"
      run_systemctl restart "$SYSTEMD_UNIT"
      echo "已重启: $SYSTEMD_UNIT"
    else
      echo "已在运行（systemd: $SYSTEMD_UNIT）"
    fi
    exit 0
  fi
  echo "通过 systemd 启动: $SYSTEMD_UNIT"
  run_systemctl start "$SYSTEMD_UNIT"
  echo "已请求启动，查看状态: sudo systemctl status $SYSTEMD_UNIT --no-pager"
  exit 0
fi

if [[ -z "$EXTERNAL_EXEC" ]]; then
  set +e
  start_core_nohup
  core_rc=$?
  set -e
  if [[ "$core_rc" -eq 0 || "$core_rc" -eq 1 ]]; then
    export EXEC_TCP="$EXEC_ADDR"
  fi
  [[ "$core_rc" -eq 0 ]] && CORE_STARTED=1
else
  export EXEC_TCP="$EXTERNAL_EXEC"
  echo "使用外部执行内核：$EXTERNAL_EXEC"
fi

if [[ ! -x "$RUST_BIN" ]]; then
  echo "未找到 Rust 二进制: $RUST_BIN"
  echo "请先执行: cargo build --release --manifest-path rust/nanoquant/Cargo.toml"
  exit 1
fi

if [[ "$DAEMON" -eq 1 ]]; then
  PID_FILE="$ROOT/data/nanoquant.pid"
  if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    if [[ "$CORE_STARTED" -eq 1 ]]; then
      echo "执行内核刚启动，重启主服务以完成握手"
      kill "$(cat "$PID_FILE")" 2>/dev/null || true
      for _ in $(seq 1 20); do
        kill -0 "$(cat "$PID_FILE")" 2>/dev/null || break
        sleep 0.5
      done
      kill -9 "$(cat "$PID_FILE")" 2>/dev/null || true
      rm -f "$PID_FILE"
    else
      echo "已在运行 (PID $(cat "$PID_FILE"))"
      exit 0
    fi
  fi
  nohup "$RUST_BIN" >>"$ROOT/logs/stdout.log" 2>&1 &
  echo $! >"$PID_FILE"
  sleep 2
  if kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    echo "已后台启动 Rust 内核 (PID $(cat "$PID_FILE")), 访问 http://127.0.0.1:$PORT"
  else
    echo "启动失败, 请查看 $ROOT/logs/stdout.log"
    exit 1
  fi
  exit 0
fi

if [[ "$CORE_STARTED" -eq 1 ]]; then
  echo "执行内核刚启动；前台主服务将直接完成握手"
fi
exec "$RUST_BIN"
