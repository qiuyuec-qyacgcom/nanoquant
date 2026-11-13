"""引擎扫描与持仓管理的主干路径（worker.py 之前只有 34% 覆盖）.

这里不去 mock 内部实现，而是给 `_scan` / `_manage_positions` 喂真实 K 线与桩化交易所：
验证"信号落库、观察模式不开仓、坏数据拦截、止盈触发平仓"这四条主干。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.engine.strategy import SignalResult
from nanoquant.exchange.client import BinanceError, Candle
from nanoquant.models import EngineEvent, PositionRecord, Signal, TradingSymbol
from nanoquant.services import state
from nanoquant.services.settings import (
    EngineSettings,
    RiskSettings,
    get_strategy,
    update_group,
)

SYMBOL = "SCANTESTUSDT"
STEP_MS = 300_000


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _candles(*, gap: bool = False, n: int = 60) -> list[Candle]:
    """以"当前时刻"为基准造 K 线：最后一根刚好收盘，避免被判成行情停更."""
    now_ms = int(time.time() * 1000)
    out: list[Candle] = []
    for i in range(n):
        if gap and i == n - 8:
            continue  # 少一根 → 制造缺口
        t = now_ms - (n - i) * STEP_MS
        price = 100.0 * (1 + 0.001 * i)
        out.append(
            Candle(open_time=t, open=price, high=price * 1.003, low=price * 0.997, close=price, volume=1000.0)
        )
    return out


def _seed_symbol(symbol: str = SYMBOL) -> None:
    with session_scope() as s:
        s.query(Signal).filter(Signal.symbol == symbol).delete()
        s.query(PositionRecord).filter(PositionRecord.symbol == symbol).delete()
        s.query(TradingSymbol).filter(TradingSymbol.symbol == symbol).delete()
        s.add(
            TradingSymbol(
                symbol=symbol, enabled=True, max_leverage=50, precision=3,
                tick_sz="0.01", min_sz="0.001", min_notional=5.0, sort_order=996,
            )
        )


def _cleanup(symbol: str = SYMBOL) -> None:
    with session_scope() as s:
        s.query(Signal).filter(Signal.symbol == symbol).delete()
        s.query(PositionRecord).filter(PositionRecord.symbol == symbol).delete()
        s.query(TradingSymbol).filter(TradingSymbol.symbol == symbol).delete()


def _broker(calls: dict | None = None):
    calls = calls if calls is not None else {}

    class _B:
        mode = "demo"

        async def account(self):
            return SimpleNamespace(equity=1000.0, available=900.0, wallet=1000.0)

        async def positions(self):
            return []

        async def open_position(self, **kw):
            calls.setdefault("open", []).append(kw)
            return SimpleNamespace(ok=True, price=kw.get("price", 0), qty=kw.get("qty", 0), message="ok")

        async def close_position(self, *, position, reason, qty=None, **kw):
            calls.setdefault("close", []).append({"symbol": position.symbol, "reason": reason, "qty": qty})
            with session_scope() as s:
                row = s.get(PositionRecord, position.id)
                if row is not None:
                    row.status = "closed"
                    row.exit_reason = reason
            return SimpleNamespace(ok=True, price=110.0, qty=qty or position.qty, message="ok")

        async def amend_stop(self, *a, **k):
            calls.setdefault("amend", []).append(k)
            return SimpleNamespace(ok=True, message="ok")

    return _B()


def _patch_market(monkeypatch, candles: list[Candle], price: float = 110.0):
    async def _tickers(symbols):
        return {
            s: SimpleNamespace(last=price, change_pct=1.0, quote_volume=1e9, source="ws")
            for s in symbols
        }

    async def _funding(symbols):
        return {s: 0.0001 for s in symbols}

    async def _oi(symbols):
        return {s: 1_000_000.0 for s in symbols}

    async def _klines(symbol, interval, limit=60, **kw):
        return candles

    monkeypatch.setattr(worker_mod.binance_client, "tickers", _tickers)
    monkeypatch.setattr(worker_mod.binance_client, "funding_rates", _funding)
    monkeypatch.setattr(worker_mod.binance_client, "open_interest", _oi)
    monkeypatch.setattr(worker_mod.binance_client, "klines", _klines)


def test_scan_records_signal_but_does_not_trade_in_observe_mode(monkeypatch):
    """auto_entry=False：信号照常落库供观察，但绝不下单。"""
    _seed_symbol()
    calls: dict = {}
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _broker(calls))
    _patch_market(monkeypatch, _candles())

    features = {"close": 110.0, "data_ok": True}
    strong = SignalResult(symbol=SYMBOL, direction="long", score=0.9, features=features, sl_pct=1.5, tp_pct=3.0)
    monkeypatch.setattr(worker_mod, "score_signal", lambda *a, **k: strong)

    old = get_strategy()
    update_group("strategy", {"auto_entry": False})
    try:
        engine = worker_mod.engine
        engine._margin.begin_cycle(900.0)
        out = asyncio.run(
            engine._scan(_broker(calls), SimpleNamespace(equity=1000.0, available=900.0),
                         EngineSettings(), RiskSettings(), get_strategy())
        )
        # 标的池是全量扫描的，别的用例可能留下启用标的：只看自己这条
        record = next(r for r in out if r["symbol"] == SYMBOL)
        assert record["direction"] == "long"
        assert "open" not in calls
        with session_scope() as s:
            row = s.query(Signal).filter(Signal.symbol == SYMBOL).order_by(Signal.ts.desc()).first()
            assert row is not None
            assert "自动开仓已关闭" in (row.reject_reason or "")
    finally:
        update_group("strategy", old.model_dump())
        _cleanup()


def test_scan_blocks_signal_when_klines_have_gap(monkeypatch):
    """坏数据必须在打分前被拦：缺口时信号记为 flat 并写明原因。"""
    _seed_symbol()
    calls: dict = {}
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _broker(calls))
    _patch_market(monkeypatch, _candles(gap=True))

    old = get_strategy()
    update_group("strategy", {"auto_entry": False, "max_data_gaps": 0, "mode": "meanrev"})
    try:
        engine = worker_mod.engine
        engine._margin.begin_cycle(900.0)
        out = asyncio.run(
            engine._scan(_broker(calls), SimpleNamespace(equity=1000.0, available=900.0),
                         EngineSettings(), RiskSettings(), get_strategy())
        )
        record = next(r for r in out if r["symbol"] == SYMBOL)
        assert "缺口" in (record.get("blocked") or "")
        assert "open" not in calls
    finally:
        update_group("strategy", old.model_dump())
        _cleanup()


def test_scan_returns_empty_when_market_data_unavailable(monkeypatch):
    _seed_symbol()

    async def _boom(symbols):
        raise BinanceError(-1001, "disconnected")

    monkeypatch.setattr(worker_mod.binance_client, "tickers", _boom)

    engine = worker_mod.engine
    engine._margin.begin_cycle(900.0)
    out = asyncio.run(
        engine._scan(_broker({}), SimpleNamespace(equity=1000.0, available=900.0),
                     EngineSettings(), RiskSettings(), get_strategy())
    )
    assert out == []
    with session_scope() as s:
        row = (
            s.query(EngineEvent)
            .filter(EngineEvent.message.like("%行情取数失败%"))
            .order_by(EngineEvent.ts.desc())
            .first()
        )
        assert row is not None
    _cleanup()


def test_manage_positions_closes_on_take_profit(monkeypatch):
    """止盈触发：管理循环要真的把平仓单发出去（worker 主干）。"""
    symbol = "MNGTESTUSDT"
    _seed_symbol(symbol)
    with session_scope() as s:
        pos = PositionRecord(
            symbol=symbol, side="long", qty=2.0, entry_price=100.0, mark_price=104.0,
            leverage=5.0, margin=40.0, sl_price=95.0, tp_price=105.0, initial_sl_price=95.0,
            peak_price=104.0, opened_at=int(time.time()) - 3600, mode="demo", status="open",
            features={}, signal_score=0.5,
        )
        s.add(pos)
        pid = None
    with session_scope() as s:
        pid = s.query(PositionRecord).filter(PositionRecord.symbol == symbol, PositionRecord.status == "open").first().id

    calls: dict = {}
    _patch_market(monkeypatch, _candles(), price=110.0)

    old = get_strategy()
    update_group("strategy", {"roi_table": [], "trailing_pct": 0.0, "partial_tp_at_r": 0.0, "max_hold_minutes": 2880})
    try:
        asyncio.run(worker_mod.engine._manage_positions(_broker(calls), get_strategy(), EngineSettings()))
        assert calls.get("close"), "止盈应当触发平仓"
        assert calls["close"][0]["reason"] == "take_profit"
        with session_scope() as s:
            row = s.get(PositionRecord, pid)
            assert row.status == "closed"
    finally:
        update_group("strategy", old.model_dump())
        _cleanup(symbol)


@pytest.fixture(autouse=True)
def _reset_engine_state():
    for key, value in (("halted", ""), ("consecutive_losses", "0"), ("sl_guard_until", "0"), ("loss_halt_until", "0")):
        state.set(key, value)
    yield
