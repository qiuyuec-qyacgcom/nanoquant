"""开仓决策链（worker._do_open）——AI 闸门、点差闸门、仓位规划的拒绝路径.

这条链路以前 0 覆盖，但它每一步都在拒绝或放行真金白银：
点差太宽要放弃、AI 否决要放弃、议会分歧要放弃、仓位不够要放弃。
任何一个分支写反，表现都是"策略看起来在跑，但永远不开仓/乱开仓"。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.ai.decision import AiDecision, DecisionOutcome
from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.exchange.broker import ExecResult
from nanoquant.exchange.precision import SymbolFilters
from nanoquant.models import Signal
from nanoquant.services.settings import get_ai, get_engine, get_risk, get_strategy, update_group

SYMBOL = "ENTRYTESTUSDT"


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _signal_row() -> int:
    with session_scope() as s:
        row = Signal(symbol=SYMBOL, ts=int(time.time()), direction="long", score=0.9, decision="open", mode="demo")
        s.add(row)
        s.flush()
        return row.id


def _reject_reason(signal_id: int) -> str:
    with session_scope() as s:
        return s.get(Signal, signal_id).reject_reason or ""


def _sig():
    return SimpleNamespace(
        symbol=SYMBOL,
        direction="long",
        score=0.82,
        sl_pct=1.2,
        tp_pct=2.4,
        features={"atr_pct": 0.8},
    )


def _ticker(price: float = 100.0):
    return SimpleNamespace(last=price, quote_volume=5e7, change_pct=1.0)


def _filters() -> SymbolFilters:
    return SymbolFilters(
        symbol=SYMBOL, price_tick=0.01, qty_step=0.001, min_qty=0.001,
        min_notional=5.0, last_price=100.0, quote_volume_24h=5e7,
    )


class _Broker:
    mode = "demo"

    def __init__(self, ok: bool = True, message: str = "") -> None:
        self.ok = ok
        self.message = message
        self.calls: list[dict] = []

    async def open_position(self, **kwargs):
        self.calls.append(kwargs)
        return ExecResult(self.ok, price=kwargs["filters"].last_price, qty=kwargs["qty"], message=self.message)


def _run(eng, broker, *, available: float = 100.0):
    """跑一次 _do_open；先给保证金台账一个预算（真实链路里由 cycle 的 begin_cycle 提供）."""
    eng._margin.begin_cycle(available)
    signal_id = _signal_row()
    try:
        asyncio.run(
            eng._do_open(
                broker, _sig(), _ticker(), SimpleNamespace(equity=100.0, available=100.0, unrealized=0.0),
                get_engine(), get_risk(), get_strategy(), signal_id,
            )
        )
        return signal_id
    finally:
        pass


@pytest.fixture(autouse=True)
def _no_ai():
    old = update_group("ai", {"enabled": False})
    yield
    update_group("ai", {"enabled": False})
    update_group("ai", old)


def test_missing_symbol_filters_is_rejected(monkeypatch):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: None)
    eng = worker_mod.TradingEngine()
    broker = _Broker()

    sid = _run(eng, broker)

    assert "不在标的池" in _reject_reason(sid)
    assert not broker.calls


def test_wide_spread_is_rejected(monkeypatch):
    """超短线利润只有千分之几，点差 30bps 做一次就白干."""
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    old = update_group("strategy", {"max_spread_bps": 10.0})

    async def _books(_symbols):
        return {SYMBOL: (99.9, 100.2)}  # 30bps

    monkeypatch.setattr(worker_mod.binance_client, "book_tickers", _books)
    eng = worker_mod.TradingEngine()
    broker = _Broker()
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": old["max_spread_bps"]})

    assert "点差" in _reject_reason(sid)
    assert not broker.calls


def test_ai_hold_vetoes_entry(monkeypatch):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    old = update_group("strategy", {"max_spread_bps": 0.0})
    update_group("ai", {"enabled": True})
    monkeypatch.setattr(worker_mod, "providers_available", lambda: 1, raising=False)
    monkeypatch.setattr("nanoquant.ai.client.providers_available", lambda: 1)

    async def _decide(_ctx):
        return DecisionOutcome(
            decision=AiDecision(action="hold", confidence=0.9, reason="结构不清晰"),
            ok=True, agreement=1.0,
        )

    monkeypatch.setattr(worker_mod, "decide", _decide)
    eng = worker_mod.TradingEngine()
    eng._build_ai_context = lambda *a, **k: {"injected_mantra_ids": []}
    broker = _Broker()
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": old["max_spread_bps"]})

    assert "AI 否决" in _reject_reason(sid)
    assert not broker.calls


def test_ai_low_agreement_vetoes_entry(monkeypatch):
    """`ai.require_agreement` 打开后，议会分歧必须真的拦住下单."""
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    old_strategy = update_group("strategy", {"max_spread_bps": 0.0})
    old_ai = update_group("ai", {"enabled": True, "require_agreement": True})
    monkeypatch.setattr("nanoquant.ai.client.providers_available", lambda: 1)

    async def _decide(_ctx):
        return DecisionOutcome(
            decision=AiDecision(action="open_long", confidence=0.9, reason="看多"),
            ok=True, agreement=0.45,
        )

    monkeypatch.setattr(worker_mod, "decide", _decide)
    eng = worker_mod.TradingEngine()
    eng._build_ai_context = lambda *a, **k: {"injected_mantra_ids": []}
    broker = _Broker()
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": old_strategy["max_spread_bps"]})
        update_group("ai", {"require_agreement": old_ai["require_agreement"]})

    assert "议会分歧" in _reject_reason(sid)
    assert not broker.calls


def test_insufficient_capital_is_rejected(monkeypatch):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    old = update_group("strategy", {"max_spread_bps": 0.0})
    eng = worker_mod.TradingEngine()
    broker = _Broker()
    eng._margin.begin_cycle(0.3)
    signal_id = _signal_row()
    try:
        asyncio.run(
            eng._do_open(
                broker, _sig(), _ticker(price=0.00002),
                SimpleNamespace(equity=0.4, available=0.3, unrealized=0.0),
                get_engine(), get_risk(), get_strategy(), signal_id,
            )
        )
    finally:
        update_group("strategy", {"max_spread_bps": old["max_spread_bps"]})

    assert "仓位不足" in _reject_reason(signal_id)
    assert not broker.calls


def test_successful_entry_passes_stop_and_target(monkeypatch):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    old = update_group("strategy", {"max_spread_bps": 0.0})
    eng = worker_mod.TradingEngine()
    broker = _Broker()
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": old["max_spread_bps"]})

    assert _reject_reason(sid) == ""
    assert len(broker.calls) == 1
    call = broker.calls[0]
    assert call["side"] == "long"
    # 止损在下方、止盈在上方，且按 tick 对齐
    assert call["sl_price"] < call["filters"].last_price < call["tp_price"]
    assert round(call["sl_price"], 2) == call["sl_price"]
    assert call["qty"] > 0


def test_failed_exchange_order_releases_margin(monkeypatch):
    """下单失败必须把预留额度还回去，否则后面几轮都开不出仓."""
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    old = update_group("strategy", {"max_spread_bps": 0.0})
    eng = worker_mod.TradingEngine()
    broker = _Broker(ok=False, message="-2019 保证金不足")
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": old["max_spread_bps"]})

    assert "保证金" in _reject_reason(sid)
    assert eng._margin.remaining >= 99.0, "失败单占用的额度没有归还"
