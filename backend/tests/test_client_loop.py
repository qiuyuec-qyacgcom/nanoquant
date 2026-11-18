"""回归：同一个进程里出现多个事件循环时，HTTP 连接池必须重建。

历史症状：`tests/test_engine_exits.py` 等用例在同一进程内多次 `asyncio.run()` 后随机报
`RuntimeError: Event loop is closed`，失败用例还会随执行顺序变化。

根因：`BinanceClient` 把 `httpx.AsyncClient` 缓存在进程级单例上，连接池绑定创建它的 loop；
第二个 `asyncio.run()` 拿到的是第一个（已关闭）loop 上的连接池。

修复：`_bind_loop()` 发现 running loop 变化就丢弃旧的连接池与 `asyncio.Lock`。
"""
from __future__ import annotations

import asyncio

from nanoquant.exchange.client import BinanceClient


def test_client_rebuilt_when_event_loop_changes():
    client = BinanceClient()

    async def grab() -> object:
        return await client._client()

    first = asyncio.run(grab())
    second = asyncio.run(grab())

    assert first is not second, "换了 loop 必须重建连接池，否则复用已关闭的连接会抛 Event loop is closed"
    assert client._http is second


def test_client_reused_within_same_loop():
    client = BinanceClient()

    async def twice() -> tuple[object, object]:
        return await client._client(), await client._client()

    a, b = asyncio.run(twice())

    assert a is b, "同一个 loop 内必须复用连接池，不能每次调用都新建"
