#!/usr/bin/env bash
# P2 出口（10.3 / 10.5 / 10.6 / 10.8）Linux + Docker 一键探针。
#
# 用途：在没有 docker / Linux 的开发机上无法完成的任务，在具备环境的主机上一条命令跑完，
#       并把原始输出与机读 JSON 落盘到 evidence/，避免再手工拼证据。
#
# 覆盖：
#   10.5  docker build 成功；镜像内不存在解释器命令；运行阶段层无解释器痕迹
#   10.6  docker compose up 后仅一个服务运行且健康，/api/v1/health 返回 kernel=rust
#   10.8  容器内完成启动、登录、demo 周期、WS 握手、备份只读
#   10.3  systemd unit 已安装时：start 后 /api/v1/health 返回 kernel=rust
#
# 用法（在 Linux 主机、仓库根目录，需 docker 与 sudo/root）：
#   bash scripts/p2_linux_docker_probe.sh
#   bash scripts/p2_linux_docker_probe.sh --skip-systemd --skip-compose
#   APP_PORT=19099 bash scripts/p2_linux_docker_probe.sh
#
# 退出码：0 = 已执行的检查全部通过且无跳过；1 = 有失败；2 = 环境不满足（非 Linux / 无 docker）。
# 该脚本不使用解释器。

set -uo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="${NANOQUANT_ROOT:-}"
if [ -z "$ROOT" ]; then
  ROOT="$SELF_DIR"
  while [ "$ROOT" != "/" ] && [ ! -d "$ROOT/.codeartsdoer/specs/rust_full_migration" ]; do
    ROOT="$(dirname "$ROOT")"
  done
fi
EVID="$ROOT/.codeartsdoer/specs/rust_full_migration/evidence"
RAW="$EVID/10-p2-linux-docker"
IMAGE="nanoquant:p2verify"
CTR="nanoquant-p2verify"
APP_PORT="${APP_PORT:-19099}"
ADMIN_PW="P2Verify123!"
DC=""
SKIP_SYSTEMD=0
SKIP_COMPOSE=0

for arg in "$@"; do
  case "$arg" in
    --skip-systemd) SKIP_SYSTEMD=1 ;;
    --skip-compose) SKIP_COMPOSE=1 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

say() { printf '[probe] %s\n' "$*"; }

# ---------- 0. 环境断言 ----------
if [ "$(uname -s)" != "Linux" ]; then
  say "not Linux: $(uname -s) -> 该探针必须在 Linux 主机执行"
  exit 2
fi
if ! command -v docker >/dev/null 2>&1; then
  say "docker not found -> 无法验证 10.5/10.6/10.8"
  exit 2
fi
docker info >/dev/null 2>&1 || { say "docker daemon not reachable"; exit 2; }
mkdir -p "$RAW"
if docker compose version >/dev/null 2>&1; then
  DC="docker compose"
elif command -v docker-compose >/dev/null 2>&1; then
  DC="docker-compose"
else
  DC=""
fi

BUILD_OK=0
INTERP_ABSENT=0
IMAGE_LAYERS_CLEAN=0
COMPOSE_STATE=skipped
SYSTEMD_STATE=skipped
CONTAINER_OK=0
CONTAINER_HEALTH=0
CONTAINER_LOGIN=0
CONTAINER_CYCLE=0
CONTAINER_WS=0
CONTAINER_BACKUP=0

# ---------- 1. 10.5 镜像构建 ----------
say "10.5 docker build $IMAGE"
docker build -t "$IMAGE" "$ROOT" >"$RAW/docker-build.log" 2>&1
if [ $? -eq 0 ]; then BUILD_OK=1; else BUILD_OK=0; fi
say "  build_ok=$BUILD_OK (log: $RAW/docker-build.log)"

if [ "$BUILD_OK" -eq 1 ]; then
  say "10.5 镜像内解释器断言"
  docker run --rm --entrypoint /bin/sh "$IMAGE" -c \
    'for c in python python3 pip pip3; do if command -v "$c" >/dev/null 2>&1; then echo "FOUND:$c"; exit 9; fi; done; echo "NO_INTERPRETER"' \
    >"$RAW/image-interpreter.txt" 2>&1
  if [ $? -eq 0 ]; then INTERP_ABSENT=1; else INTERP_ABSENT=0; fi
  say "  interpreter_absent=$INTERP_ABSENT"

  docker history --no-trunc --format '{{.CreatedBy}}' "$IMAGE" >"$RAW/image-history.txt" 2>&1
  if grep -Eiq 'python|/pip|pip3' "$RAW/image-history.txt"; then
    IMAGE_LAYERS_CLEAN=0
    grep -Ein 'python|/pip|pip3' "$RAW/image-history.txt" >"$RAW/image-history-hits.txt"
  else
    IMAGE_LAYERS_CLEAN=1
  fi
  say "  runtime_layers_clean=$IMAGE_LAYERS_CLEAN"

  docker image inspect "$IMAGE" >"$RAW/image-inspect.json" 2>&1
fi

# ---------- 2. 10.3 systemd ----------
if command -v systemd-analyze >/dev/null 2>&1 && [ -f "$ROOT/deploy/nanoquant.service" ]; then
  TMP_UNIT="$(mktemp --suffix=.service)"
  sed "s#<REPO_ROOT>#$ROOT#g" "$ROOT/deploy/nanoquant.service" >"$TMP_UNIT"
  systemd-analyze verify "$TMP_UNIT" >"$RAW/systemd-analyze-verify.log" 2>&1
  say "10.3 systemd-analyze verify rc=$? (informational, see $RAW/systemd-analyze-verify.log)"
  rm -f "$TMP_UNIT"
fi
if [ "$SKIP_SYSTEMD" -eq 1 ]; then
  say "10.3 skipped by flag"
elif ! command -v systemctl >/dev/null 2>&1; then
  say "10.3 skipped: no systemctl"
elif ! systemctl list-unit-files 2>/dev/null | grep -q '^nanoquant\.service'; then
  say "10.3 skipped: nanoquant.service not installed (run scripts/install.sh --systemd first)"
else
  say "10.3 systemd start nanoquant"
  systemctl daemon-reload >"$RAW/systemd-daemon-reload.log" 2>&1
  systemctl restart nanoquant >"$RAW/systemd-restart.log" 2>&1
  sleep 5
  systemctl status nanoquant --no-pager >"$RAW/systemd-status.txt" 2>&1
  SYS_PORT="$(cat "$ROOT/data/.port" 2>/dev/null | tr -d '[:space:]')"
  case "$SYS_PORT" in
    ''|*[!0-9]*) SYS_PORT=8899 ;;
  esac
  SYS_HEALTH="$(curl -fsS --max-time 5 "http://127.0.0.1:$SYS_PORT/api/v1/health" 2>/dev/null)"
  printf '%s' "$SYS_HEALTH" >"$RAW/systemd-health.json"
  if printf '%s' "$SYS_HEALTH" | grep -Eq '"kernel"[[:space:]]*:[[:space:]]*"rust"'; then
    SYSTEMD_STATE=pass
  else
    SYSTEMD_STATE=fail
  fi
  say "  systemd_state=$SYSTEMD_STATE"
  systemctl stop nanoquant >"$RAW/systemd-stop.log" 2>&1
fi

# ---------- 3. 10.6 compose ----------
if [ "$SKIP_COMPOSE" -eq 1 ]; then
  COMPOSE_STATE=skipped
elif [ -z "$DC" ]; then
  say "10.6 skipped: docker compose plugin not found"
else
  say "10.6 compose up"
  mkdir -p "$ROOT/data" "$ROOT/logs"
  if [ "$(id -u)" = "0" ]; then
    chown -R 1000:1000 "$ROOT/data" "$ROOT/logs" 2>/dev/null
  else
    DATA_UID="$(stat -c %u "$ROOT/data" 2>/dev/null || echo 0)"
    if [ "$DATA_UID" != "1000" ]; then
      say "  warn: $ROOT/data 属主 uid=$DATA_UID，容器内 nanoquant(uid 1000) 可能无写权限"
      say "        可先执行: sudo chown -R 1000:1000 $ROOT/data $ROOT/logs"
    fi
  fi
  ( cd "$ROOT" && $DC up -d --build ) >"$RAW/compose-up.log" 2>&1
  UP_RC=$?
  if [ "$UP_RC" -eq 0 ]; then
    for _ in $(seq 1 90); do
      ST="$(docker inspect -f '{{.State.Health.Status}}' nanoquant 2>/dev/null || echo missing)"
      [ "$ST" = "healthy" ] && break
      sleep 2
    done
    ( cd "$ROOT" && $DC ps ) >"$RAW/compose-ps.txt" 2>&1
    SVCS="$(cd "$ROOT" && $DC ps --services 2>/dev/null | tr -d '\r' | grep -c .)"
    printf '%s' "$SVCS" >"$RAW/compose-service-count.txt"
    COMPOSE_HEALTH="$(curl -fsS --max-time 5 'http://127.0.0.1:8899/api/v1/health' 2>/dev/null)"
    printf '%s' "$COMPOSE_HEALTH" >"$RAW/compose-health.json"
    if [ "$ST" = "healthy" ] && [ "$SVCS" = "1" ] && printf '%s' "$COMPOSE_HEALTH" | grep -Eq '"kernel"[[:space:]]*:[[:space:]]*"rust"'; then
      COMPOSE_STATE=pass
    else
      COMPOSE_STATE=fail
    fi
    ( cd "$ROOT" && $DC logs --no-color --tail 100 nanoquant ) >"$RAW/compose-logs.txt" 2>&1
  else
    COMPOSE_STATE=fail
  fi
  say "  compose_state=$COMPOSE_STATE"
  ( cd "$ROOT" && $DC down ) >"$RAW/compose-down.log" 2>&1
fi

# ---------- 4. 10.8 容器内全链路 ----------
say "10.8 container scenario (port $APP_PORT)"
docker rm -f "$CTR" >/dev/null 2>&1
HOME_DIR="$(mktemp -d)"
mkdir -p "$HOME_DIR/data" "$HOME_DIR/logs"
chmod -R 0777 "$HOME_DIR"

if [ "$BUILD_OK" -ne 1 ]; then
  say "  skipped: no image"
else
  docker run -d --name "$CTR" \
    -p "$APP_PORT:8899" \
    -e NANOQUANT_HOME=/app \
    -e NANOQUANT_PORT=8899 \
    -e "NANOQUANT_ADMIN_PASSWORD=$ADMIN_PW" \
    -v "$HOME_DIR/data:/app/data" \
    -v "$HOME_DIR/logs:/app/logs" \
    "$IMAGE" >"$RAW/container-run.txt" 2>&1

  for _ in $(seq 1 120); do
    H="$(curl -fsS --max-time 2 "http://127.0.0.1:$APP_PORT/api/v1/health" 2>/dev/null)"
    if printf '%s' "$H" | grep -Eq '"ok"[[:space:]]*:[[:space:]]*true'; then break; fi
    sleep 1
  done
  printf '%s' "$H" >"$RAW/container-health.json"
  if printf '%s' "$H" | grep -Eq '"kernel"[[:space:]]*:[[:space:]]*"rust"'; then CONTAINER_HEALTH=1; fi

  LOGIN="$(curl -fsS --max-time 10 -X POST "http://127.0.0.1:$APP_PORT/api/v1/auth/login" \
    -H 'Content-Type: application/json' \
    -d "{\"username\":\"admin\",\"password\":\"$ADMIN_PW\"}" 2>/dev/null)"
  printf '%s' "$LOGIN" >"$RAW/container-login.json"
  TOKEN="$(printf '%s' "$LOGIN" | sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
  if [ -n "$TOKEN" ]; then CONTAINER_LOGIN=1; fi

  if [ -n "$TOKEN" ]; then
    CYCLE_CODE="$(curl -s -o "$RAW/container-cycle-now.json" -w '%{http_code}' --max-time 10 \
      -X POST -H "Authorization: Bearer $TOKEN" \
      "http://127.0.0.1:$APP_PORT/api/v1/engine/cycle-now")"
    printf '%s' "$CYCLE_CODE" >"$RAW/container-cycle-code.txt"
    [ "$CYCLE_CODE" = "202" ] && CONTAINER_CYCLE=1

    WS_CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 4 \
      -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
      -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
      "http://127.0.0.1:$APP_PORT/api/v1/ws")"
    printf '%s' "$WS_CODE" >"$RAW/container-ws-code.txt"
    [ "$WS_CODE" = "101" ] && CONTAINER_WS=1

    BK_CODE="$(curl -s -o "$RAW/container-backup-options.json" -w '%{http_code}' --max-time 10 \
      -H "Authorization: Bearer $TOKEN" \
      "http://127.0.0.1:$APP_PORT/api/v1/backup/options")"
    printf '%s' "$BK_CODE" >"$RAW/container-backup-code.txt"
    [ "$BK_CODE" = "200" ] && CONTAINER_BACKUP=1

    curl -s --max-time 10 -H "Authorization: Bearer $TOKEN" \
      "http://127.0.0.1:$APP_PORT/api/v1/system/diagnostics" >"$RAW/container-diagnostics.json" 2>&1
  fi

  docker logs --tail 80 "$CTR" >"$RAW/container-logs.txt" 2>&1
  docker rm -f "$CTR" >/dev/null 2>&1
  rm -rf "$HOME_DIR"
fi

if [ "$CONTAINER_HEALTH" -eq 1 ] && [ "$CONTAINER_LOGIN" -eq 1 ] && [ "$CONTAINER_CYCLE" -eq 1 ] \
   && [ "$CONTAINER_WS" -eq 1 ] && [ "$CONTAINER_BACKUP" -eq 1 ]; then
  CONTAINER_OK=1
fi
say "  container_ok=$CONTAINER_OK (health=$CONTAINER_HEALTH login=$CONTAINER_LOGIN cycle=$CONTAINER_CYCLE ws=$CONTAINER_WS backup=$CONTAINER_BACKUP)"

# ---------- 5. 汇总 ----------
bool() { if [ "$1" -eq 1 ]; then printf 'true'; else printf 'false'; fi; }
FINAL_OK=0
COMPLETE=0
if [ "$BUILD_OK" -eq 1 ] && [ "$INTERP_ABSENT" -eq 1 ] && [ "$IMAGE_LAYERS_CLEAN" -eq 1 ] \
   && [ "$CONTAINER_OK" -eq 1 ] && [ "$COMPOSE_STATE" != "fail" ] && [ "$SYSTEMD_STATE" != "fail" ]; then
  FINAL_OK=1
fi
if [ "$FINAL_OK" -eq 1 ] && [ "$COMPOSE_STATE" = "pass" ] && [ "$SYSTEMD_STATE" = "pass" ]; then
  COMPLETE=1
fi

cat >"$EVID/10-p2-exit-linux.json" <<EOF
{
  "task": "10.3,10.5,10.6,10.8",
  "platform": "linux",
  "container_runtime": "$(docker version --format '{{.Server.Version}}' 2>/dev/null)",
  "generated_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "image": "$IMAGE",
  "checks": {
    "docker_build": $(bool "$BUILD_OK"),
    "image_interpreter_absent": $(bool "$INTERP_ABSENT"),
    "image_runtime_layers_clean": $(bool "$IMAGE_LAYERS_CLEAN"),
    "systemd": "$SYSTEMD_STATE",
    "compose": "$COMPOSE_STATE",
    "container_health_kernel_rust": $(bool "$CONTAINER_HEALTH"),
    "container_login": $(bool "$CONTAINER_LOGIN"),
    "container_cycle_now_202": $(bool "$CONTAINER_CYCLE"),
    "container_ws_101": $(bool "$CONTAINER_WS"),
    "container_backup_options_200": $(bool "$CONTAINER_BACKUP")
  },
  "ok": $(bool "$FINAL_OK"),
  "complete": $(bool "$COMPLETE"),
  "raw_dir": "$RAW"
}
EOF

say "result: ok=$FINAL_OK complete=$COMPLETE -> $EVID/10-p2-exit-linux.json"
if [ "$COMPLETE" -eq 1 ]; then exit 0; else exit 1; fi