"""回归：sidecar 代发保护单必须真拿到凭证，而不是在模块上找 current_credentials。

历史 bug：`broker.py::_try_sidecar_protection` 写的是 `binance_client.current_credentials()`，
但 `binance_client` 是**模块**（`from . import client as binance_client`），单例叫 `client`。
AttributeError 被外层 `except Exception` 吞掉 → 该函数永远返回 False，
P3-3 的「sidecar 代发保护单」形同关闭，只在日志里留一条 warning，没人发现。
"""
from __future__ import annotations

import asyncio
from types import SimpleNamespace

from nanoquant.exchange import broker as broker_mod
from nanoquant.exchange.broker import BinanceBroker
from nanoquant.services.order_ledger import order_ledger


def test_sidecar_protection_uses_client_credentials(monkeypatch):
    seen: dict[str, object] = {}

    monkeypatch.setattr(order_ledger, "place_enabled", lambda: True)

    async def fake_place(**kwargs):
        seen.update(kwargs)
        return {"ok": True}

    monkeypatch.setattr(order_ledger, "place_protection", fake_place)
    monkeypatch.setattr(
        broker_mod.binance_client,
        "client",
        SimpleNamespace(current_credentials=lambda: ("KEY123", "SECRET456")),
    )

    broker = BinanceBroker()
    got = asyncio.run(
        broker._try_sidecar_protection(
            symbol="BTCUSDT",
            close_side="SELL",
            sl_price=73000.0,
            tp_price=75000.0,
            side_params={},
            fmt=lambda v: f"{v:g}",
        )
    )

    assert got is True, "sidecar 已成功代发时必须返回 True，否则调用方会再发一套保护单"
    assert seen.get("api_key") == "KEY123", "凭证必须来自 binance_client.client 单例"
    assert seen.get("stop_price") == "73000"
    assert seen.get("take_price") == "75000"
