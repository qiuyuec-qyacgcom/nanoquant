"""手动仓接管：没有保护单时按 ATR 补挂止损/止盈.

背景：手动开的仓不走系统下单链路，交易所侧可能根本没挂止损，对账只会显示 sl=0。
补挂走与自动开仓相同的 amend_stop（先撤旧单再重挂），并按策略倍数算价格。
"""
from __future__ import annotations

import asyncio
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.exchange.client import Candle
from nanoquant.models import PositionRecord
from nanoquant.services.settings import get_strategy, update_group


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _seed(symbol: str = "PROTUSDT", side: str = "long", qty: float = 10.0, entry: float = 100.0) -> int:
    with session_scope() as s:
        s.query(PositionRecord).filter(PositionRecord.symbol == symbol).delete()
        row = PositionRecord(
            symbol=symbol, side=side, qty=qty, entry_price=entry, mark_price=entry,
            leverage=4.0, margin=qty * entry / 4.0, opened_at=0, mode="demo", status="open",
            ai_reason="test",
        )
        s.add(row)
    with session_scope() as s:
        found = (
            s.query(PositionRecord)
            .filter(PositionRecord.symbol == symbol, PositionRecord.status == "open")
            .first()
        )
        return int(found.id)


def _candles(high: float = 102.0, low: float = 98.0) -> list[Candle]:
    return [
        Candle(open_time=i * 900_000, open=100.0, high=high, low=low, close=100.0, volume=1.0)
        for i in range(60)
    ]


def test_protect_position_uses_atr_and_writes_back(monkeypatch):
    symbol = "PROTUSDT"
    pid = _seed(symbol)
    old = get_strategy()
    update_group("strategy", {"timeframe": "15m", "atr_period": 14, "sl_atr_mult": 1.5, "tp_atr_mult": 2.0})

    calls: dict[str, float | str] = {}

    class _Broker:
        async def amend_stop(self, symbol, side, stop, qty, tp_price=0.0):
            calls.update(symbol=symbol, side=side, stop=stop, qty=qty, tp=tp_price)
            return SimpleNamespace(ok=True, message="ok")

    async def _klines(symbol, interval="15m", limit=120, **kwargs):
        return _candles()

    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(worker_mod.binance_client, "klines", _klines)

    try:
        out = asyncio.run(worker_mod.engine.protect_position(pid))
    finally:
        update_group("strategy", old.model_dump())

    assert out["ok"] is True
    # ATR = 4（high-low），1.5 倍止损 → 94；2.0 倍止盈 → 108
    assert out["sl_price"] == pytest.approx(94.0, rel=1e-6)
    assert out["tp_price"] == pytest.approx(108.0, rel=1e-6)
    assert calls["qty"] == pytest.approx(10.0)
    assert calls["side"] == "long"

    with session_scope() as s:
        row = s.get(PositionRecord, pid)
        assert row is not None
        assert row.sl_price == pytest.approx(94.0, rel=1e-6)
        assert row.tp_price == pytest.approx(108.0, rel=1e-6)
        s.delete(row)


def test_protect_position_refuses_without_atr(monkeypatch):
    pid = _seed("PROTFLATUSDT")

    class _Broker:
        async def amend_stop(self, *args, **kwargs):  # pragma: no cover - 不应被调用
            raise AssertionError("ATR 不可用时不该下单")

    async def _klines(symbol, interval="15m", limit=120, **kwargs):
        return _candles(high=100.0, low=100.0)  # 全平 → TR=0

    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(worker_mod.binance_client, "klines", _klines)

    out = asyncio.run(worker_mod.engine.protect_position(pid))

    assert out["ok"] is False
    assert "ATR" in out["reason"]

    with session_scope() as s:
        row = s.get(PositionRecord, pid)
        if row is not None:
            s.delete(row)


def test_protect_position_rejects_missing_position():
    out = asyncio.run(worker_mod.engine.protect_position(999_999))
    assert out["ok"] is False
    assert "不存在" in out["reason"]
