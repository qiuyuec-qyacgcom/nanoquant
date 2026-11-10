"""外部信号跟单：状态 / 事件 / HTTP 直推 / 立即读一轮 / 游标重置.

文件轮询对接 ao-monitor 的 `signals.jsonl`；上游也可以把同一份 JSON 直接
POST 到 `/signal-follow/ingest` 做到**信号到达即刻跟仓**（零轮询延迟）。
两条路径共用同一套幂等、风控、资金与保证金账本。

直推鉴权：配置了 `ingest_token` 后可用 `X-Signal-Token` 供机器调用；
未配置或密钥不匹配时回落到后台登录态，不额外开放匿名下单入口。
"""
from __future__ import annotations

import secrets
from typing import Any

from fastapi import APIRouter, Body, Depends, Header, Query

from ...db import session_scope
from ...engine.worker import engine
from ...models import SignalFollow
from ...services.settings import get_signal_follow
from ..deps import current_user

router = APIRouter(tags=["signal_follow"])


def _ingest_actor(
    x_signal_token: str = Header(default=""),
    authorization: str = Header(default=""),
) -> str:
    cfg = get_signal_follow()
    if cfg.ingest_token and secrets.compare_digest(x_signal_token or "", cfg.ingest_token):
        return "signal-bot"
    return current_user(authorization)


@router.get("/signal-follow/status")
def follow_status(user: str = Depends(current_user)) -> dict[str, Any]:
    return engine.signal_follow_status()


@router.get("/signal-follow/events")
def follow_events(
    limit: int = Query(default=50, ge=1, le=500),
    status: str = Query(default=""),
    user: str = Depends(current_user),
) -> dict[str, Any]:
    with session_scope() as s:
        q = s.query(SignalFollow).order_by(SignalFollow.created_at.desc())
        if status:
            q = q.filter(SignalFollow.status == status)
        rows = q.limit(limit).all()
        items = [
            {
                "id": r.id, "signal_id": r.signal_id, "symbol": r.symbol, "side": r.side,
                "status": r.status, "reason": r.reason, "leverage": r.leverage,
                "qty": r.qty, "price": r.price, "margin": r.margin,
                "mode": r.mode, "created_at": r.created_at,
            }
            for r in rows
        ]
    return {"items": items}


@router.post("/signal-follow/ingest")
async def follow_ingest(
    payload: dict[str, Any] = Body(...),
    actor: str = Depends(_ingest_actor),
) -> dict[str, Any]:
    """上游直接把一条信号推过来（等价于 signals.jsonl 的一行），不等待轮询."""
    return await engine.follow_signal_now(payload)


@router.post("/signal-follow/read-once")
async def follow_read_once(user: str = Depends(current_user)) -> dict[str, Any]:
    """立即读取一轮 signals.jsonl（排查链路用，不等 poll_sec）."""
    count = await engine._poll_signal_feed_once()  # noqa: SLF001
    return {"ok": True, "processed": count, **engine.signal_follow_status()}


@router.post("/signal-follow/reset-cursor")
def follow_reset_cursor(user: str = Depends(current_user)) -> dict[str, Any]:
    """把当前频道游标移到文件尾部：切换频道 / 丢弃积压旧信号时使用，不回放历史."""
    return engine.reset_follow_cursor()
