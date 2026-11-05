"""币安 24h Ticker WebSocket 流.

动机:
- REST `/fapi/v1/ticker/24hr` 权重 40; 引擎每 5 秒一轮 + 前台/后台报价都依赖它,
  长期跑下来是权重预算里最大的一笔, 也是之前触发 418 封禁的主因之一。
- WebSocket `symbol@ticker` 推送同样的 24h 滚动统计, 不占 REST 权重,
  服务端缓存最新快照, 供 `client.tickers()` 优先读取; REST 只做首帧/断流兜底。
"""
from __future__ import annotations

import asyncio
import json
import time
from typing import Any

import websockets

from ..logging_setup import get_logger
from ..services.settings import get_engine, normalize_mode
from .client import Ticker

log = get_logger("ticker_stream")

STREAM_HOSTS = {
    "demo": "wss://stream.binancefuture.com/ws",
    "live": "wss://fstream.binance.com/ws",
}

#: 快照可用的新鲜度窗口 (秒); 超过则视为陈旧, 调用方回退 REST
FRESH_SEC = 15.0
#: 超过该时长未被请求的标的自动退订 (池子收缩后释放上游订阅)
IDLE_UNSUBSCRIBE_SEC = 300.0


class TickerStreamHub:
    def __init__(self) -> None:
        self._snap: dict[str, Ticker] = {}
        self._ts: dict[str, float] = {}
        self._subs: set[str] = set()
        self._requested: dict[str, float] = {}
        self._task: asyncio.Task | None = None
        self._ws: Any = None
        self._env = ""
        self._lock = asyncio.Lock()
        self._backoff = 1.0
        self._connected = False

    @property
    def connected(self) -> bool:
        return self._connected

    def status(self) -> dict[str, Any]:
        return {
            "connected": self._connected,
            "env": self._env,
            "symbols": sorted(self._subs),
            "fresh": len(self._snap),
        }

    async def ensure_subscribed(self, symbols: list[str]) -> None:
        wanted = {s.upper() for s in symbols if s}
        now = time.time()
        async with self._lock:
            added = [s for s in wanted if s not in self._subs]
            for s in wanted:
                self._requested[s] = now
            self._subs |= wanted
            self._ensure_task()
            ws = self._ws
        if added and ws is not None:
            await self._send(ws, [f"{s.lower()}@ticker" for s in added], "SUBSCRIBE")

    def snapshot(self) -> dict[str, Ticker]:
        now = time.time()
        return {s: t for s, t in self._snap.items() if now - self._ts.get(s, 0) < FRESH_SEC}

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
            log.warning("Ticker 流 %s 发送失败: %s", method, str(exc)[:120])

    def _ensure_task(self) -> None:
        if self._task is None or self._task.done():
            self._task = asyncio.create_task(self._run(), name="ticker-stream")

    def _dispatch(self, raw: str | bytes) -> None:
        try:
            msg = json.loads(raw)
        except (TypeError, ValueError):
            return
        data = msg.get("data") if isinstance(msg, dict) else None
        if not isinstance(data, dict):
            data = msg
        if not isinstance(data, dict) or data.get("e") != "24hrTicker":
            return
        symbol = str(data.get("s") or "")
        if not symbol:
            return
        try:
            ticker = Ticker(
                symbol=symbol,
                last=float(data.get("c") or 0),
                change_pct=float(data.get("P") or 0),
                high=float(data.get("h") or 0),
                low=float(data.get("l") or 0),
                quote_volume=float(data.get("q") or 0),
                ts=int(data.get("E") or time.time() * 1000),
            )
        except (TypeError, ValueError):
            return
        self._snap[symbol] = ticker
        self._ts[symbol] = time.time()

    def _prune(self, now: float) -> list[str]:
        stale = [s for s, t in list(self._requested.items()) if now - t > IDLE_UNSUBSCRIBE_SEC and s in self._subs]
        return stale

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
                    streams = [f"{s.lower()}@ticker" for s in sorted(self._subs)]
                    await self._send(ws, streams, "SUBSCRIBE")
                    log.info("Ticker 流已连接 %s (%d 个订阅)", url, len(streams))
                    last_prune = time.time()
                    while True:
                        if normalize_mode(get_engine().mode) != self._env:
                            break
                        try:
                            raw = await asyncio.wait_for(ws.recv(), timeout=30)
                        except asyncio.TimeoutError:
                            raw = None
                        if raw is not None:
                            self._dispatch(raw)
                        now = time.time()
                        if now - last_prune > 60:
                            last_prune = now
                            stale = self._prune(now)
                            if stale:
                                async with self._lock:
                                    for s in stale:
                                        self._subs.discard(s)
                                        self._requested.pop(s, None)
                                        self._snap.pop(s, None)
                                        self._ts.pop(s, None)
                                await self._send(ws, [f"{s.lower()}@ticker" for s in stale], "UNSUBSCRIBE")
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001
                log.warning("Ticker 流断开: %s", str(exc)[:160])
            finally:
                self._ws = None
                self._connected = False
            await asyncio.sleep(self._backoff)
            self._backoff = min(15.0, self._backoff * 2)


ticker_hub = TickerStreamHub()
