"""WebSocket 推送: 事件流 + K 线流.

鉴权约定：token 不再放进 URL 查询串（那会进 access log / 反向代理日志 /
浏览器历史）。改为连接后第一帧发 ``{"type": "auth", "token": "..."}``。
"""
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


async def _auth_first_frame(websocket: WebSocket, timeout: float = 5.0) -> dict | None:
    """连接后读取首帧鉴权，返回 token payload 或 None.

    首帧约定 ``{"type": "auth", "token": "<token>"}``。收到其它帧 / 超时 /
    token 无效，一律返回 None，由调用方决定是 4401 还是降级匿名。
    """
    cfg = get_security()
    try:
        raw = await asyncio.wait_for(websocket.receive_text(), timeout=timeout)
    except (asyncio.TimeoutError, WebSocketDisconnect, RuntimeError):
        return None
    try:
        msg = json.loads(raw)
    except (json.JSONDecodeError, ValueError):
        return None
    if not isinstance(msg, dict) or msg.get("type") != "auth":
        return None
    return verify_token(str(msg.get("token", "")), cfg.token_version)


@router.websocket("/ws")
async def ws_endpoint(websocket: WebSocket) -> None:
    cfg = get_security()
    await websocket.accept()
    if cfg.require_login and await _auth_first_frame(websocket) is None:
        await websocket.close(code=4401)
        return
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
) -> None:
    """K 线推送: 浏览器只连本服务, 由服务端复用一条币安 WebSocket 流.

    不占用 REST 权重, 因此不会因为刷新 K 线把出口 IP 打进封禁.
    """
    target = (symbol or "").upper()
    if not target or interval not in ALLOWED_INTERVALS:
        await websocket.close(code=4400)
        return

    cfg = get_security()
    await websocket.accept()
    # 可选鉴权：尽力读一个短窗口的首帧，有合法 token 就按管理员放行；
    # 没有则按匿名处理，site_enabled 关闭时匿名一律 4403。
    is_admin = not cfg.require_login
    if cfg.require_login:
        is_admin = await _auth_first_frame(websocket, timeout=0.6) is not None
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
