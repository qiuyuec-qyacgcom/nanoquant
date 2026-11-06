"""系统: 健康检查 / 一键诊断 / 配置中心 / 事件与日志."""
from __future__ import annotations

import platform
import shutil
import sys
import time

from fastapi import APIRouter, Depends, HTTPException, Query

from ... import __version__
from ...config import APP_NAME, paths
from ...db import session_scope
from ...exchange.broker import get_broker
from ...exchange.client import client as binance_client
from ...logging_setup import get_logger
from ...models import AiCallLog, AiProvider, EngineEvent, Signal, WriteFailure
from ...services import events, state
from ...services import backup as backup_service
from ...services.settings import (
    GROUPS,
    SettingsValidationError,
    get_ai,
    get_all,
    get_data,
    get_engine,
    get_group,
    get_system,
    normalize_mode,
    reset_group,
    update_group,
)
from ..deps import current_user
from ..schemas import CredentialsIn, SettingsIn

router = APIRouter(tags=["system"])
log = get_logger("api.system")

STARTED_AT = int(time.time())


@router.get("/health")
def health() -> dict:
    return {"ok": True, "app": APP_NAME, "version": __version__, "ts": int(time.time())}


@router.get("/system/info")
def info(user: str = Depends(current_user)) -> dict:
    engine_state = state.as_dict()
    return {
        "app": APP_NAME,
        "version": __version__,
        "python": sys.version.split()[0],
        "platform": platform.platform(),
        "started_at": STARTED_AT,
        "uptime": int(time.time()) - STARTED_AT,
        "paths": {
            "home": str(paths.HOME),
            "data": str(paths.DATA),
            "logs": str(paths.LOGS),
            "backups": str(paths.BACKUPS),
        },
        "engine": {
            "enabled": engine_state.get("enabled") == "true",
            "mode": normalize_mode(engine_state.get("mode")),
            "heartbeat": engine_state.get("last_cycle_ts"),
        },
        "settings_groups": list(GROUPS.keys()),
    }


@router.get("/system/diagnostics")
async def diagnostics(user: str = Depends(current_user)) -> dict:
    checks: list[dict] = []

    # 1. 数据库
    started = time.perf_counter()
    try:
        with session_scope() as s:
            events_count = s.query(EngineEvent).count()
            signals_count = s.query(Signal).count()
            providers = s.query(AiProvider).all()
            ai_ok = s.query(AiCallLog).filter(AiCallLog.ok.is_(True)).count()
            ai_fail = s.query(AiCallLog).filter(AiCallLog.ok.is_(False)).count()
        sys_cfg = get_system()
        checks.append(
            {
                "name": "数据库",
                "ok": True,
                "detail": (
                    f"SQLite 正常, 事件 {events_count} 条 (保留 {sys_cfg.event_retention_days} 天), "
                    f"信号 {signals_count} 条 (保留 {sys_cfg.signal_retention_days} 天), "
                    f"调用成功/失败 {ai_ok}/{ai_fail}"
                ),
                "latency_ms": int((time.perf_counter() - started) * 1000),
            }
        )
    except Exception as exc:  # noqa: BLE001
        checks.append({"name": "数据库", "ok": False, "detail": str(exc)[:200], "latency_ms": 0})
        providers = []

    # 2. 写库队列 (异步落库的积压/失败/慢写)
    try:
        from ...services.db_writer import BACKLOG_WARN, db_writer

        stats = db_writer.stats()
        queue_ok = stats["pending"] <= BACKLOG_WARN and stats["failed"] == 0
        detail = (
            f"积压 {stats['pending']} 条 / 未完成 {stats['unfinished']} · "
            f"累计写入 {stats['total']} 次 · 失败 {stats['failed']} · 最近一次 {stats['last_ms']}ms"
        )
        if stats["last_error"]:
            detail += f" · 最近错误: {stats['last_error'][:80]}"
        checks.append(
            {
                "name": "写库队列",
                "ok": queue_ok,
                "detail": detail,
                "latency_ms": int(stats["last_ms"]),
            }
        )
    except Exception as exc:  # noqa: BLE001
        checks.append({"name": "写库队列", "ok": False, "detail": str(exc)[:160], "latency_ms": 0})

    # 3. 落库补偿 (异步写库最终失败)
    try:
        with session_scope() as s:
            dead_count = s.query(WriteFailure).filter(WriteFailure.resolved.is_(False)).count()
        checks.append(
            {
                "name": "落库补偿",
                "ok": dead_count == 0,
                "detail": (
                    "无待补偿记录"
                    if dead_count == 0
                    else f"{dead_count} 条待补偿记录，可在「落库补偿」页查看并重放"
                ),
                "latency_ms": 0,
            }
        )
    except Exception as exc:  # noqa: BLE001
        checks.append({"name": "落库补偿", "ok": False, "detail": str(exc)[:160], "latency_ms": 0})

    # 4. 交易所接入 (当前交易环境: 模拟盘=测试网 / 实盘=主网)
    try:
        mode = normalize_mode(get_engine().mode)
        data_cfg = get_data()
        if mode == "demo":
            key_enc, secret_enc, env_label = (
                data_cfg.demo_api_key_enc,
                data_cfg.demo_api_secret_enc,
                "模拟盘 · 币安测试网",
            )
        else:
            key_enc, secret_enc, env_label = (
                data_cfg.live_api_key_enc,
                data_cfg.live_api_secret_enc,
                "实盘 · 币安主网",
            )
        from ...security import decrypt_secret

        has_key = bool(decrypt_secret(key_enc)) and bool(decrypt_secret(secret_enc))
        started = time.perf_counter()
        reachable = False
        if has_key:
            reachable = await binance_client.client.ping()
        checks.append(
            {
                "name": "交易所接入",
                "ok": has_key and reachable,
                "detail": (
                    f"{env_label} · 接口连通"
                    if has_key and reachable
                    else (f"{env_label} · 未配置 API 凭证" if not has_key else f"{env_label} · 接口不可达")
                ),
                "latency_ms": int((time.perf_counter() - started) * 1000),
                "extra": {"env": mode, "has_credentials": has_key, "reachable": reachable},
            }
        )
    except Exception as exc:  # noqa: BLE001
        checks.append({"name": "交易所接入", "ok": False, "detail": str(exc)[:200], "latency_ms": 0})

    # 5. 交易通道
    try:
        broker = get_broker()
        bh = await broker.health()
        checks.append(
            {
                "name": f"交易通道 ({broker.mode})",
                "ok": bool(bh.get("ok")),
                "detail": str(bh.get("detail"))[:200],
                "latency_ms": 0,
                "extra": bh,
            }
        )
    except Exception as exc:  # noqa: BLE001
        checks.append({"name": "交易通道", "ok": False, "detail": str(exc)[:200], "latency_ms": 0})

    # 6. 引擎
    last_cycle = state.get_int("last_cycle_ts", 0)
    age = int(time.time()) - last_cycle if last_cycle else -1
    engine_on = state.get("enabled") == "true"
    if not engine_on:
        checks.append(
            {
                "name": "引擎心跳",
                "ok": True,
                "detail": "引擎已停止 (未启用时不需要心跳)",
                "latency_ms": 0,
                "extra": {"last_error": state.get("last_error"), "halted": state.get("halted")},
            }
        )
    else:
        checks.append(
            {
                "name": "引擎心跳",
                "ok": age >= 0 and age < 120,
                "detail": f"最近一轮 {age}s 前" if age >= 0 else "引擎已启用但尚未产生心跳",
                "latency_ms": 0,
                "extra": {"last_error": state.get("last_error"), "halted": state.get("halted")},
            }
        )

    # 7. AI
    enabled_providers = [p for p in providers if p.enabled]
    ai_cfg = get_ai()
    checks.append(
        {
            "name": "AI 模型",
            "ok": (not ai_cfg.enabled) or bool(enabled_providers),
            "detail": (
                f"已启用 {len(enabled_providers)} 个模型"
                if enabled_providers
                else ("AI 已关闭, 纯量化运行" if not ai_cfg.enabled else "AI 已开启但未配置模型, 当前回退纯量化")
            ),
            "latency_ms": 0,
            "extra": {"providers": [{"id": p.id, "name": p.name, "model": p.model} for p in providers]},
        }
    )

    # 8. 磁盘
    try:
        usage = shutil.disk_usage(paths.DATA)
        backup_usage = backup_service.disk_usage()
        checks.append(
            {
                "name": "存储",
                "ok": usage.free > 200 * 1024 * 1024,
                "detail": f"剩余 {usage.free / 1024**3:.1f} GB, 备份占用 {backup_usage['bytes'] / 1024**2:.1f} MB",
                "latency_ms": 0,
            }
        )
    except OSError as exc:
        checks.append({"name": "存储", "ok": False, "detail": str(exc), "latency_ms": 0})

    return {
        "ok": all(c["ok"] for c in checks),
        "checks": checks,
        "checked_at": int(time.time()),
        "version": __version__,
    }


@router.get("/settings")
def read_settings(user: str = Depends(current_user)) -> dict:
    return {"groups": get_all(), "defaults": {name: model().model_dump() for name, model in GROUPS.items()}}


@router.get("/settings/{group}")
def read_group(group: str, user: str = Depends(current_user)) -> dict:
    try:
        return {"group": group, "data": get_group(group, use_cache=False)}
    except KeyError as exc:
        raise HTTPException(status_code=404, detail=f"未知配置组 {group}") from exc


@router.put("/settings/{group}")
def write_group(group: str, payload: SettingsIn, user: str = Depends(current_user)) -> dict:
    # 交易环境只走 /engine/control (带确认短语), 防止绕过确认直接改配置
    if group == "engine" and "mode" in payload.patch:
        if normalize_mode(payload.patch.get("mode")) != normalize_mode(get_engine().mode):
            raise HTTPException(
                status_code=400,
                detail="切换交易环境请使用「交易环境」开关（实盘需要输入确认短语 LIVE）",
            )
    try:
        data = update_group(group, payload.patch)
    except KeyError as exc:
        raise HTTPException(status_code=404, detail=f"未知配置组 {group}") from exc
    except SettingsValidationError as exc:
        raise HTTPException(status_code=400, detail=exc.detail) from exc
    events.record("info", "settings", f"{user} 更新配置组 {group}", {"patch": payload.patch})
    return {"ok": True, "group": group, "data": data}


@router.post("/settings/{group}/reset")
def reset(group: str, user: str = Depends(current_user)) -> dict:
    try:
        data = reset_group(group)
    except KeyError as exc:
        raise HTTPException(status_code=404, detail=f"未知配置组 {group}") from exc
    events.record("warn", "settings", f"{user} 重置配置组 {group} 为默认值")
    return {"ok": True, "group": group, "data": data}


@router.get("/system/events")
def list_events(
    limit: int = Query(default=100, ge=1, le=500),
    level: str = "",
    category: str = "",
    user: str = Depends(current_user),
) -> dict:
    with session_scope() as s:
        q = s.query(EngineEvent).order_by(EngineEvent.ts.desc())
        if level:
            q = q.filter(EngineEvent.level == level)
        if category:
            q = q.filter(EngineEvent.category == category)
        rows = q.limit(limit).all()
        items = [
            {"id": r.id, "ts": r.ts, "level": r.level, "category": r.category, "message": r.message, "payload": r.payload}
            for r in rows
        ]
    return {"items": items, "live": events.recent(30)}


@router.get("/system/logs")
def logs(lines: int = Query(default=200, ge=10, le=2000), keyword: str = "", user: str = Depends(current_user)) -> dict:
    path = paths.LOGS / "nanoquant.log"
    if not path.exists():
        return {"lines": [], "path": str(path)}
    content = path.read_text(encoding="utf-8", errors="ignore").splitlines()
    if keyword:
        content = [c for c in content if keyword.lower() in c.lower()]
    return {"lines": content[-lines:], "path": str(path)}


@router.get("/system/system-settings")
def system_settings(user: str = Depends(current_user)) -> dict:
    return {"data": get_system()}


@router.get("/system/archives")
def list_archives(user: str = Depends(current_user)) -> dict:
    """历史归档文件列表 (retention 清理前写出的 gzip JSONL)."""
    import gzip
    import re

    from ...services.retention import ARCHIVE_DIR

    items = []
    if ARCHIVE_DIR.exists():
        for path in sorted(ARCHIVE_DIR.glob("*.jsonl.gz"), key=lambda p: p.stat().st_mtime, reverse=True)[:50]:
            match = re.match(r"^([a-z]+)-(\d{8})\.jsonl\.gz$", path.name)
            if not match:
                continue
            stat = path.stat()
            items.append(
                {
                    "name": path.name,
                    "kind": match.group(1),
                    "date": match.group(2),
                    "size": stat.st_size,
                    "mtime": int(stat.st_mtime),
                }
            )
    return {"items": items, "dir": str(ARCHIVE_DIR)}


@router.get("/system/archives/{name}")
def preview_archive(
    name: str,
    limit: int = Query(default=20, ge=1, le=200),
    user: str = Depends(current_user),
) -> dict:
    """读取归档文件的最后 N 行 (按时间倒序返回), 用于快速回看."""
    import gzip
    import json
    import re

    from ...services.retention import ARCHIVE_DIR

    if not re.match(r"^[a-z]+-\d{8}\.jsonl\.gz$", name):
        raise HTTPException(status_code=400, detail="文件名不合法")
    path = (ARCHIVE_DIR / name).resolve()
    if ARCHIVE_DIR.resolve() not in path.parents or not path.exists():
        raise HTTPException(status_code=404, detail="归档文件不存在")

    rows: list[dict] = []
    try:
        with gzip.open(path, "rt", encoding="utf-8") as fh:
            lines = fh.readlines()[-limit:]
    except OSError as exc:
        raise HTTPException(status_code=500, detail=f"归档读取失败: {exc}") from exc
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    rows.reverse()
    return {"name": name, "count": len(rows), "rows": rows}


@router.get("/system/write-health")
def write_health(user: str = Depends(current_user)) -> dict:
    """写库健康: 队列统计 + 慢写 + 死信, 并给一句结论.

    死信主要来自两类原因: 锁竞争 (慢写多但失败少) 与磁盘抖动 (慢写与 max 同时飙高)。
    把两者放在同一个响应里, 才能判断该查 SQL 还是查磁盘。
    """
    from ...services import dead_letter
    from ...services.db_writer import BACKLOG_WARN, SLOW_WRITE_MS, db_writer

    stats = db_writer.stats()
    dlq = dead_letter.summary()
    slow_ratio = round(stats["slow"] / stats["total"] * 100, 2) if stats["total"] else 0.0

    if stats["pending"] > BACKLOG_WARN:
        verdict = "队列积压, 检查是否有慢查询或写事务未提交"
    elif dlq["pending"] and stats["slow"]:
        verdict = "有死信且存在慢写, 优先排查磁盘 IO 与锁竞争"
    elif dlq["pending"]:
        verdict = "有死信但无慢写, 更可能是并发写冲突或字段约束"
    elif stats["max_ms"] > SLOW_WRITE_MS:
        verdict = "偶发慢写但已自动恢复, 继续观察"
    else:
        verdict = "正常"

    return {
        "queue": stats,
        "dead_letters": dlq,
        "slow_ratio_pct": slow_ratio,
        "thresholds": {"backlog": BACKLOG_WARN, "slow_ms": SLOW_WRITE_MS},
        "verdict": verdict,
    }


@router.get("/system/dead-letters")
def list_dead_letters(
    limit: int = Query(default=20, ge=1, le=200), user: str = Depends(current_user)
) -> dict:
    """写库最终失败的死信列表 (未处理)."""
    with session_scope() as s:
        total = s.query(WriteFailure).filter(WriteFailure.resolved.is_(False)).count()
        rows = (
            s.query(WriteFailure)
            .filter(WriteFailure.resolved.is_(False))
            .order_by(WriteFailure.ts.desc())
            .limit(limit)
            .all()
        )
        items = [
            {
                "id": row.id,
                "ts": row.ts,
                "kind": row.kind,
                "error": row.error,
                "attempts": row.attempts,
                "payload": row.payload,
            }
            for row in rows
        ]
    return {"items": items, "total": total}


@router.post("/system/dead-letters/{failure_id}/replay")
def replay_dead_letter(failure_id: int, user: str = Depends(current_user)) -> dict:
    """重放一条死信 (带 client_order_id 幂等检查)."""
    from ...services import dead_letter

    result = dead_letter.replay_one(failure_id)
    if result.get("status") == "missing":
        raise HTTPException(status_code=404, detail="死信不存在")
    if result.get("status") == "resolved":
        raise HTTPException(status_code=409, detail="该死信已处理")
    if not result.get("ok"):
        raise HTTPException(status_code=400, detail=result.get("detail") or "重放失败")
    return result


@router.post("/system/dead-letters/{failure_id}/ignore")
def ignore_dead_letter(failure_id: int, user: str = Depends(current_user)) -> dict:
    """人工忽略一条死信 (仅标记, 不写库)."""
    from ...services import dead_letter

    result = dead_letter.ignore_one(failure_id)
    if not result.get("ok"):
        raise HTTPException(status_code=404, detail=result.get("detail") or "死信不存在")
    return result


@router.post("/system/dead-letters/replay-all")
def replay_all_dead_letters(
    limit: int = Query(default=50, ge=1, le=200), user: str = Depends(current_user)
) -> dict:
    """批量重放未处理的死信 (默认最多 50 条)."""
    from ...services import dead_letter

    return dead_letter.replay_all(limit)


@router.get("/system/alerts/logs")
def alert_logs(limit: int = Query(default=20, ge=1, le=200), user: str = Depends(current_user)) -> dict:
    """最近告警发送记录 (确认通道是否真的通)."""
    from ...models import AlertLog

    with session_scope() as s:
        rows = s.query(AlertLog).order_by(AlertLog.ts.desc()).limit(limit).all()
        items = [
            {
                "id": row.id,
                "ts": row.ts,
                "level": row.level,
                "category": row.category,
                "message": row.message,
                "channels": row.channels,
                "ok": row.ok,
                "error": row.error,
            }
            for row in rows
        ]
    return {"items": items}


@router.post("/system/alerts/test")
async def test_alert(
    level: str = Query(default="warn", pattern="^(info|warn|error)$"),
    user: str = Depends(current_user),
) -> dict:
    """发一条测试告警 (可指定级别, 用于验证分级路由), 返回每个通道的结果."""
    from ...services import alerts

    results = await alerts.send(level, "test", f"这是一条 {level} 测试告警：收到即表示配置正确。")
    if not results:
        raise HTTPException(status_code=400, detail="未配置任何告警通道（Webhook 或 Telegram）")
    return {"ok": all(item.get("ok") for item in results), "results": results}


@router.get("/settings/credentials/exchange")
def exchange_credentials(user: str = Depends(current_user)) -> dict:
    """两个交易环境各自的凭证状态 (模拟盘=测试网 / 实盘=主网)."""
    from ...security import decrypt_secret, mask_secret

    cfg = get_data()

    def view(key_enc: str, secret_enc: str) -> dict:
        key = decrypt_secret(key_enc)
        secret = decrypt_secret(secret_enc)
        return {
            "has_key": bool(key),
            "has_secret": bool(secret),
            "configured": bool(key) and bool(secret),
            "api_key_masked": mask_secret(key),
            "api_secret_masked": mask_secret(secret),
        }

    return {
        "demo": view(cfg.demo_api_key_enc, cfg.demo_api_secret_enc),
        "live": view(cfg.live_api_key_enc, cfg.live_api_secret_enc),
        "env": normalize_mode(get_engine().mode),
        "proxy": cfg.proxy,
        "demo_base_url": cfg.demo_base_url,
        "live_base_url": cfg.live_base_url,
    }


@router.post("/settings/credentials/exchange")
def save_exchange_credentials(payload: CredentialsIn, user: str = Depends(current_user)) -> dict:
    from ...security import encrypt_secret

    env = normalize_mode(payload.env, default="live")
    prefix = "demo" if env == "demo" else "live"
    patch: dict = {}
    if payload.api_key:
        patch[f"{prefix}_api_key_enc"] = encrypt_secret(payload.api_key.strip())
    if payload.api_secret:
        patch[f"{prefix}_api_secret_enc"] = encrypt_secret(payload.api_secret.strip())
    if payload.proxy is not None:
        patch["proxy"] = payload.proxy.strip()
    if not patch:
        raise HTTPException(status_code=400, detail="没有需要保存的内容")
    update_group("data", patch)
    events.record("warn", "settings", f"{user} 更新{env}环境交易所接入参数", {"fields": list(patch.keys())})
    return {"ok": True, "env": env, "updated": list(patch.keys())}


@router.delete("/settings/credentials/exchange")
def clear_exchange_credentials(env: str = "live", user: str = Depends(current_user)) -> dict:
    target = normalize_mode(env, default="live")
    prefix = "demo" if target == "demo" else "live"
    update_group("data", {f"{prefix}_api_key_enc": "", f"{prefix}_api_secret_enc": ""})
    events.record("warn", "settings", f"{user} 清空{target}环境的交易所 API 密钥")
    return {"ok": True, "env": target}
