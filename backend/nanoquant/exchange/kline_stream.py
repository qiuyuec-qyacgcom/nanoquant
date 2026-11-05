"""币安 K 线 WebSocket 流.

为什么要走 WS:
- REST `/fapi/v1/klines` 每 3 秒轮询会把出口 IP 打进 418 封禁 (实测过,
  封禁期间整块面板取不到任何数据);
- WebSocket 推送没有 REST 权重, 一根蜡烛的每次变动都会即时推送;
- 历史 K 线只在「切标的 / 切周期」时取一次 (权重 2), 之后全靠推送增量更新.

设计:
- 一个上游连接 (`/ws`), 按需动态 SUBSCRIBE/UNSUBSCRIBE;
- 多个浏览器订阅同一 (symbol, interval) 时复用同一条流, 扇出到各自的队列;
- 断线指数退避重连, 重连后自动补订阅;
- 交易环境切换 (demo/live) 时自动切换到对应的流域名.
"""
from __future__ import annotations

import asyncio
import json
import time
from typing import Any

import websockets

from ..logging_setup import get_logger
from ..services.settings import get_engine, normalize_mode

log = get_logger("kline_stream")

#: 币安 U 本位永续行情推送 (模拟盘=测试网, 实盘=主网)
STREAM_HOSTS = {
    "demo": "wss://stream.binancefuture.com/ws",
    "live": "wss://fstream.binance.com/ws",
}

ALLOWED_INTERVALS = {"1m", "3m", "5m", "15m", "30m", "1h", "4h", "1d"}


def stream_name(symbol: str, interval: str) -> str:
    return f"{symbol.lower()}@kline_{interval}"


class KlineStreamHub:
    def __init__(self) -> None:
        self._subs: dict[tuple[str, str], set[asyncio.Queue]] = {}
        self._task: asyncio.Task | None = None
        self._ws: Any = None
        self._env = ""
        self._lock = asyncio.Lock()
        self._backoff = 1.0
        self._last_msg_ts = 0.0
        self._connected = False

    # ------------------------------------------------------------- public api
    @property
    def connected(self) -> bool:
        return self._connected

    def status(self) -> dict[str, Any]:
        return {
            "connected": self._connected,
            "env": self._env,
            "subscriptions": [f"{s}@{i}" for s, i in self._subs],
            "clients": sum(len(q) for q in self._subs.values()),
            "last_message_age": round(time.time() - self._last_msg_ts, 1) if self._last_msg_ts else None,
        }

    async def subscribe(self, symbol: str, interval: str) -> asyncio.Queue:
        symbol = symbol.upper()
        key = (symbol, interval)
        queue: asyncio.Queue = asyncio.Queue(maxsize=256)
        async with self._lock:
            self._subs.setdefault(key, set()).add(queue)
            self._ensure_task()
            ws = self._ws
            if ws is not None:
                await self._send(ws, [stream_name(symbol, interval)], "SUBSCRIBE")
        return queue

    async def unsubscribe(self, symbol: str, interval: str, queue: asyncio.Queue) -> None:
        key = (symbol.upper(), interval)
        async with self._lock:
            subs = self._subs.get(key)
            if subs is None:
                return
            subs.discard(queue)
            if subs:
                return
            self._subs.pop(key, None)
            ws = self._ws
            if ws is not None:
                await self._send(ws, [stream_name(*key)], "UNSUBSCRIBE")

    async def close(self) -> None:
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
            self._task = None
        self._connected = False

    # ------------------------------------------------------------- internals
    @staticmethod
    async def _send(ws: Any, streams: list[str], method: str) -> None:
        try:
            await ws.send(json.dumps({"method": method, "params": streams, "id": int(time.time() * 1000)}))
        except Exception as exc:  # noqa: BLE001
            log.warning("K线流 %s 发送失败: %s", method, str(exc)[:120])

    def _ensure_task(self) -> None:
        if self._task is None or self._task.done():
            self._task = asyncio.create_task(self._run(), name="kline-stream")

    def _dispatch(self, raw: str | bytes) -> None:
        try:
            msg = json.loads(raw)
        except (TypeError, ValueError):
            return
        # 原始 /ws 用 SUBSCRIBE 订阅时, kline 事件在顶层; 组合流才包在 data 里
        data = msg.get("data") if isinstance(msg, dict) else None
        if not isinstance(data, dict):
            data = msg
        if not isinstance(data, dict) or data.get("e") != "kline":
            return
        k = data.get("k") or {}
        try:
            payload = {
                "symbol": str(k["s"]).upper(),
                "interval": str(k["i"]),
                "t": int(k["t"]),
                "o": float(k["o"]),
                "h": float(k["h"]),
                "l": float(k["l"]),
                "c": float(k["c"]),
                "v": float(k["v"]),
                "closed": bool(k["x"]),
                "next_close_at": int(k["T"]) + 1,
                "event_time": int(data.get("E") or time.time() * 1000),
            }
        except (KeyError, TypeError, ValueError):
            return
        self._last_msg_ts = time.time()
        for queue in list(self._subs.get((payload["symbol"], payload["interval"]), ())):
            try:
                queue.put_nowait(payload)
            except asyncio.QueueFull:
                pass

    async def _run(self) -> None:
        while True:
            if not self._subs:
                await asyncio.sleep(0.4)
                continue
            env = normalize_mode(get_engine().mode)
            url = STREAM_HOSTS.get(env, STREAM_HOSTS["demo"])
            try:
                async with websockets.connect(
                    url, ping_interval=20, ping_timeout=20, open_timeout=10, max_queue=512
                ) as ws:
                    self._ws = ws
                    self._env = env
                    self._connected = True
                    self._backoff = 1.0
                    streams = [stream_name(s, i) for s, i in list(self._subs)]
                    if streams:
                        await self._send(ws, streams, "SUBSCRIBE")
                    log.info("K线流已连接 %s (%d 个订阅)", url, len(streams))
                    while True:
                        if normalize_mode(get_engine().mode) != self._env:
                            log.info("交易环境变化, 重连 K 线流")
                            break
                        try:
                            raw = await asyncio.wait_for(ws.recv(), timeout=30)
                        except asyncio.TimeoutError:
                            continue
                        self._dispatch(raw)
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001
                log.warning("K线流断开: %s", str(exc)[:160])
            finally:
                self._ws = None
                self._connected = False
            await asyncio.sleep(self._backoff)
            self._backoff = min(15.0, self._backoff * 2)


kline_hub = KlineStreamHub()
