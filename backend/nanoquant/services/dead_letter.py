"""死信队列的查看、单条重放、批量重放与忽略.

幂等前提:
- 订单类写入带 `client_order_id`, 重放前先查 orders 表;
- 已存在同 `client_order_id` 的记录 → 视为已落库, 直接标记 resolved, **不重复插入**.
  这避免了"重试成功但进程崩溃、重启后重放导致重复记账"。
"""
from __future__ import annotations

from typing import Any

from sqlalchemy import func

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import OrderRecord, WriteFailure
from . import events

log = get_logger("dead_letter")


def _registry() -> dict[str, Any]:
    from ..exchange.broker import _persist_close, _persist_entry

    return {"_persist_entry": _persist_entry, "_persist_close": _persist_close}


def _payload_cid(kind: str, payload: dict[str, Any]) -> str:
    args = payload.get("args") or []
    if kind in {"_persist_entry", "_persist_close"} and args and isinstance(args[0], dict):
        return str(args[0].get("client_order_id") or "")
    return ""


def already_applied(kind: str, payload: dict[str, Any]) -> bool:
    cid = _payload_cid(kind, payload)
    if not cid:
        return False
    with session_scope() as s:
        return s.query(OrderRecord).filter(OrderRecord.client_order_id == cid).count() > 0


def _mark(failure_id: int, resolved: bool = True) -> None:
    with session_scope() as s:
        row = s.get(WriteFailure, failure_id)
        if row is not None:
            row.resolved = resolved


def replay_one(failure_id: int, *, auto: bool = False) -> dict[str, Any]:
    """重放一条死信; 返回 {ok, status, detail}。

    `auto=True` 表示由定时任务触发: 失败时会累加 `auto_attempts`, 达到上限后不再自动尝试。
    """
    with session_scope() as s:
        row = s.get(WriteFailure, failure_id)
        if row is None:
            return {"ok": False, "status": "missing", "detail": "死信不存在"}
        if row.resolved:
            return {"ok": False, "status": "resolved", "detail": "该死信已处理"}
        kind = row.kind
        payload = row.payload or {}

    handler = _registry().get(kind)
    if handler is None:
        return {"ok": False, "status": "unsupported", "detail": f"不支持重放的类型: {kind}"}

    if already_applied(kind, payload):
        _mark(failure_id)
        events.record("info", "db", f"死信 #{failure_id} 幂等跳过 (client_order_id 已落库)", {"failure_id": failure_id})
        return {"ok": True, "status": "skipped", "detail": "同 client_order_id 已落库, 直接标记为已处理"}

    args = tuple(payload.get("args") or [])
    kwargs = payload.get("kwargs") or {}
    try:
        handler(*args, **kwargs)
    except Exception as exc:  # noqa: BLE001
        log.warning("死信重放失败 #%s: %s", failure_id, exc)
        if auto:
            with session_scope() as s:
                row = s.get(WriteFailure, failure_id)
                if row is not None:
                    row.auto_attempts = (row.auto_attempts or 0) + 1
        return {"ok": False, "status": "failed", "detail": str(exc)[:200]}

    _mark(failure_id)
    events.record("info", "db", f"死信 #{failure_id} 重放成功", {"failure_id": failure_id})
    return {"ok": True, "status": "replayed", "detail": "已重放"}


def ignore_one(failure_id: int) -> dict[str, Any]:
    with session_scope() as s:
        row = s.get(WriteFailure, failure_id)
        if row is None:
            return {"ok": False, "status": "missing", "detail": "死信不存在"}
        row.resolved = True
    events.record("warn", "db", f"死信 #{failure_id} 已被人工忽略", {"failure_id": failure_id})
    return {"ok": True, "status": "ignored", "detail": "已标记为处理"}


AUTO_MAX_ATTEMPTS = 3


def auto_replay(limit: int = 20) -> dict[str, Any]:
    """定时任务调用: 只处理可重放且未超自动尝试上限的死信."""
    with session_scope() as s:
        ids = [
            row[0]
            for row in s.query(WriteFailure.id)
            .filter(
                WriteFailure.resolved.is_(False),
                WriteFailure.auto_attempts < AUTO_MAX_ATTEMPTS,
            )
            .order_by(WriteFailure.ts.asc())
            .limit(limit)
            .all()
        ]
    stats = {"total": len(ids), "replayed": 0, "skipped": 0, "failed": 0, "unsupported": 0}
    for failure_id in ids:
        result = replay_one(failure_id, auto=True)
        status = result.get("status", "failed")
        if status in stats:
            stats[status] += 1
        elif not result.get("ok"):
            stats["failed"] += 1
    return stats


def summary() -> dict[str, Any]:
    """死信概览 (给总览面板用)."""
    with session_scope() as s:
        pending = s.query(WriteFailure).filter(WriteFailure.resolved.is_(False)).count()
        stuck = (
            s.query(WriteFailure)
            .filter(
                WriteFailure.resolved.is_(False),
                WriteFailure.auto_attempts >= AUTO_MAX_ATTEMPTS,
            )
            .count()
        )
        last_row = (
            s.query(WriteFailure.ts).order_by(WriteFailure.ts.desc()).first()
        )
        grouped = (
            s.query(WriteFailure.kind, func.count(WriteFailure.id))
            .filter(WriteFailure.resolved.is_(False))
            .group_by(WriteFailure.kind)
            .order_by(func.count(WriteFailure.id).desc())
            .all()
        )
    return {
        "pending": pending,
        "stuck": stuck,
        "last_ts": int(last_row[0]) if last_row else 0,
        "by_kind": [{"kind": kind or "unknown", "count": count} for kind, count in grouped],
    }


def replay_all(limit: int = 50) -> dict[str, Any]:
    """批量重放未处理的死信 (默认最多 50 条)."""
    with session_scope() as s:
        ids = [
            row[0]
            for row in s.query(WriteFailure.id)
            .filter(WriteFailure.resolved.is_(False))
            .order_by(WriteFailure.ts.asc())
            .limit(limit)
            .all()
        ]
    stats = {"total": len(ids), "replayed": 0, "skipped": 0, "failed": 0, "unsupported": 0}
    for failure_id in ids:
        result = replay_one(failure_id)
        status = result.get("status", "failed")
        if status in stats:
            stats[status] += 1
        elif not result.get("ok"):
            stats["failed"] += 1
    return stats
