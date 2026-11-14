"""运行事件总线: 落库 + 内存环形缓冲 + WebSocket 订阅者推送."""
from __future__ import annotations

import asyncio
import time
from collections import deque
from typing import Any

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import EngineEvent

log = get_logger("events")

_RING: deque[dict[str, Any]] = deque(maxlen=400)
_SUBSCRIBERS: set[asyncio.Queue] = set()
_SEQ = 0
#: 告警推送失败的上次告警时间（秒），避免推送通道故障时每个事件都刷一条日志
_ALERT_FAIL_TS = 0.0
_ALERT_FAIL_INTERVAL = 300.0


def subscribe() -> asyncio.Queue:
    q: asyncio.Queue = asyncio.Queue(maxsize=500)
    _SUBSCRIBERS.add(q)
    return q


def unsubscribe(q: asyncio.Queue) -> None:
    _SUBSCRIBERS.discard(q)


def _fanout(payload: dict[str, Any]) -> None:
    for q in list(_SUBSCRIBERS):
        try:
            q.put_nowait(payload)
        except asyncio.QueueFull:
            _SUBSCRIBERS.discard(q)


def record(
    level: str,
    category: str,
    message: str,
    payload: dict[str, Any] | None = None,
    *,
    persist: bool = True,
) -> dict[str, Any]:
    global _SEQ
    _SEQ += 1
    item = {
        "seq": _SEQ,
        "level": level,
        "category": category,
        "message": message,
        "payload": payload or {},
        "ts": int(__import__("time").time()),
    }
    _RING.append(item)
    if level in {"warn", "error"}:
        log.warning("[%s] %s", category, message)
    else:
        log.info("[%s] %s", category, message)
    if persist:
        try:
            with session_scope() as s:
                s.add(
                    EngineEvent(
                        ts=item["ts"],
                        level=level,
                        category=category,
                        message=message[:2000],
                        payload=payload or None,
                    )
                )
        except Exception as exc:  # pragma: no cover - 落库失败不影响主流程
            log.error("事件落库失败: %s", exc)
    _fanout({"type": "event", "data": item})
    # 告警推送: 内部已做级别过滤与同类冷却, 失败不影响主流程
    try:
        from .alerts import on_event

        on_event(level, category, message)
    except Exception:  # noqa: BLE001
        global _ALERT_FAIL_TS
        now = time.monotonic()
        if now - _ALERT_FAIL_TS > _ALERT_FAIL_INTERVAL:
            _ALERT_FAIL_TS = now
            # 静默吞掉这条等于"告警通道坏了, 但没人知道", 5 分钟最多提示一次
            log.warning("告警推送失败(5 分钟内只提示一次)", exc_info=True)
    return item


def recent(limit: int = 80) -> list[dict[str, Any]]:
    return list(_RING)[-limit:][::-1]


def publish(kind: str, data: Any) -> None:
    """向所有在线前端推送自定义消息 (不经数据库)."""
    _fanout({"type": kind, "data": data})
