"""WebSocket 推送: 事件流 + K 线流."""
from __future__ import annotations

import asyncio
import json

from fastapi import APIRouter, WebSocket, WebSocketDisconnect

from ...exchange.kline_stream import ALLOWED_INTERVALS, kline_hub
from ...logging_setup import get_logger
from ...security import verify_token
from ...services import events
from ...services.settings import get_public, get_security
from ...services.symbols import ordered_symbols

router = APIRouter()
log = get_logger("ws")


@router.websocket("/ws")
async def ws_endpoint(websocket: WebSocket, token: str = "") -> None:
    cfg = get_security()
    if cfg.require_login and not verify_token(token):
        await websocket.close(code=4401)
        return
    await websocket.accept()
    queue = events.subscribe()
    try:
        await websocket.send_text(json.dumps({"type": "hello", "data": {"ok": True}}, ensure_ascii=False))
        while True:
            try:
                item = await asyncio.wait_for(queue.get(), timeout=25)
                await websocket.send_text(json.dumps(item, ensure_ascii=False, default=str))
            except asyncio.TimeoutError:
                await websocket.send_text(json.dumps({"type": "ping", "data": {}}))
    except (WebSocketDisconnect, RuntimeError):
        pass
    finally:
        events.unsubscribe(queue)


@router.websocket("/ws/klines")
async def ws_klines(
    websocket: WebSocket,
    symbol: str = "",
    interval: str = "1m",
    token: str = "",
) -> None:
    """K 线推送: 浏览器只连本服务, 由服务端复用一条币安 WebSocket 流.

    不占用 REST 权重, 因此不会因为刷新 K 线把出口 IP 打进封禁.
    """
    target = (symbol or "").upper()
    if not target or interval not in ALLOWED_INTERVALS:
        await websocket.close(code=4400)
        return

    cfg = get_security()
    is_admin = (not cfg.require_login) or bool(verify_token(token))
    if not is_admin and not get_public().site_enabled:
        await websocket.close(code=4403)
        return
    try:
        pool = ordered_symbols(enabled_only=True)
    except Exception:  # noqa: BLE001
        pool = []
    if target not in pool:
        await websocket.close(code=4404)
        return

    await websocket.accept()
    queue = await kline_hub.subscribe(target, interval)
    try:
        await websocket.send_json(
            {"type": "hello", "data": {"symbol": target, "interval": interval}}
        )
        while True:
            try:
                bar = await asyncio.wait_for(queue.get(), timeout=20)
            except asyncio.TimeoutError:
                await websocket.send_json({"type": "ping"})
                continue
            await websocket.send_json({"type": "kline", "data": bar})
    except (WebSocketDisconnect, RuntimeError):
        pass
    finally:
        await kline_hub.unsubscribe(target, interval, queue)
