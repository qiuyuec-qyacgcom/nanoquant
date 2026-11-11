# -*- coding: utf-8 -*-
"""跟单生命周期事件：加仓 / 减仓 / 平仓 / 移止损。

这四类以前统一被当成"非开仓事件"忽略，实测后果是：
频道喊清仓时系统一直拿着裸仓，喊加仓时纹丝不动，喊移保本时止损还在原处。
本文件锁住它们各自该做的事，以及"没有持仓时绝不凭空下单"这条底线。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

import nanoquant.engine.worker as worker
from nanoquant.db import init_db, session_scope
from nanoquant.models import PositionRecord, SignalFollow, TradingSymbol
from nanoquant.services import state
from nanoquant.services import symbols as symbol_service
from nanoquant.services.settings import get_group, update_group

SYMBOL = "LCYCLEUSDT"


@pytest.fixture
def env(monkeypatch):
    """一个 100 USDT、10x 的 BTC 式多单，价格 100。"""
    init_db()
    with session_scope() as s:
        s.query(PositionRecord).filter(PositionRecord.symbol == SYMBOL).delete()
        s.query(SignalFollow).filter(SignalFollow.symbol == SYMBOL).delete()
        s.query(TradingSymbol).filter(TradingSymbol.symbol == SYMBOL).delete()
        s.add(
            TradingSymbol(
                symbol=SYMBOL, enabled=True, max_leverage=100, precision=3,
                tick_sz="0.01", min_sz="0.001", min_notional=5.0, sort_order=998,
            )
        )
        s.add(
            PositionRecord(
                symbol=SYMBOL, side="long", qty=2.0, entry_price=100.0,
                mark_price=110.0, leverage=10.0, margin=20.0,
                sl_price=95.0, tp_price=120.0, peak_price=110.0,
                opened_at=int(time.time()) - 60, mode="demo", status="open",
            )
        )

    calls: dict[str, list] = {"add": [], "close": [], "amend": []}

    class FakeBroker:
        mode = "demo"

        async def account(self):
            return SimpleNamespace(
                equity=1000.0, available=900.0, wallet=1000.0,
                unrealized=0.0, margin_used=0.0, as_dict=lambda: {},
            )

        async def add_to_position(self, **kw):
            calls["add"].append(kw)
            return SimpleNamespace(ok=True, price=110.0, qty=kw["qty"], message="加仓成功")

        async def close_position(self, **kw):
            calls["close"].append(kw)
            return SimpleNamespace(ok=True, price=110.0, qty=kw.get("qty") or 2.0, message="平仓")

        async def amend_stop(self, symbol, side, stop_price, qty, tp_price=0.0):
            calls["amend"].append(
                {"symbol": symbol, "side": side, "stop": stop_price,
                 "qty": qty, "tp": tp_price}
            )
            return SimpleNamespace(ok=True, price=stop_price, message="ok")

    async def fake_tickers(symbols):  # noqa: ARG001
        return {SYMBOL: SimpleNamespace(last=110.0, change_pct=1.0, quote_volume=1e9)}

    monkeypatch.setattr(worker.binance_client, "tickers", fake_tickers)
    monkeypatch.setattr(worker, "get_broker", lambda mode: FakeBroker())  # noqa: ARG005
    for key in ("halted", "consecutive_losses", "loss_halt_until", "sl_guard_until"):
        state.set(key, "" if key == "halted" else "0")

    eng = worker.engine
    old_lock = eng._follow_lock
    eng._follow_lock = asyncio.Lock()
    old_engine = get_group("engine", use_cache=False)
    old_follow = get_group("signal_follow", use_cache=False)
    update_group("engine", {"enabled": True})
    update_group("signal_follow", {"enabled": True, "dry_run": False, "leverage_bonus": 10})

    try:
        yield eng, calls
    finally:
        eng._follow_lock = old_lock
        update_group("engine", old_engine)
        update_group("signal_follow", old_follow)
        with session_scope() as s:
            s.query(PositionRecord).filter(PositionRecord.symbol == SYMBOL).delete()
            s.query(SignalFollow).filter(SignalFollow.symbol == SYMBOL).delete()
            s.query(TradingSymbol).filter(TradingSymbol.symbol == SYMBOL).delete()


def _signal(event, **over):
    row = {"id": f"lc-{event}-{time.time_ns()}", "symbol": SYMBOL, "side": "long",
           "event": event, "ts": int(time.time())}
    row.update(over)
    return row


def _run(eng, row):
    return asyncio.run(eng.follow_signal_now(row))


def test_close_closes_the_position(env):
    eng, calls = env
    res = _run(eng, _signal("close"))
    assert res["status"] == "closed"
    assert len(calls["close"]) == 1
    # 全平不传 qty，由 broker 取当时账本里的数量
    assert calls["close"][0].get("qty") is None


def test_reduce_closes_only_the_given_pct(env):
    eng, calls = env
    res = _run(eng, _signal("reduce", reduce_pct=50))
    assert res["status"] == "reduced"
    assert len(calls["close"]) == 1
    assert calls["close"][0]["qty"] == pytest.approx(1.0)  # 2.0 的一半


def test_reduce_without_pct_defaults_to_half(env):
    eng, calls = env
    res = _run(eng, _signal("reduce"))
    assert res["status"] == "reduced"
    assert calls["close"][0]["qty"] == pytest.approx(1.0)


def test_move_stop_to_breakeven_uses_own_entry_price(env):
    """'止损移到入场价'必须用我们自己的均价，不是频道写的那个数字。"""
    eng, calls = env
    res = _run(eng, _signal("move_stop", stop_to_breakeven=True))
    assert res["status"] == "stop_moved"
    call = calls["amend"][0]
    assert call["stop"] == pytest.approx(100.0)  # 本地 entry_price
    # cancel_all 会撤掉止盈，所以必须把 tp 一起重挂
    assert call["tp"] == pytest.approx(120.0)


def test_move_stop_absolute_price(env):
    eng, calls = env
    res = _run(eng, _signal("move_stop", sl=104.0))
    assert res["status"] == "stop_moved"
    assert calls["amend"][0]["stop"] == 104.0


def test_move_stop_above_market_is_refused(env):
    """多单止损挂到现价之上会立即触发平仓，必须拦下。"""
    eng, calls = env
    res = _run(eng, _signal("move_stop", sl=115.0))  # 现价 110
    assert res["status"] == "skipped"
    assert "立即触发" in res["reason"]
    assert calls["amend"] == []


def test_add_sizes_by_margin_pct_and_keeps_stop(env):
    eng, calls = env
    res = _run(eng, _signal("add"))
    assert res["status"] == "added"
    kw = calls["add"][0]
    assert kw["qty"] > 0
    # 信号没给新止损时沿用原仓位的止损，绝不能变成裸仓
    assert kw["sl_price"] == pytest.approx(95.0)
    assert kw["tp_price"] == pytest.approx(120.0)


def test_add_uses_new_stop_when_signal_provides_one(env):
    eng, calls = env
    res = _run(eng, _signal("add", sl=103.0, tp=130.0))
    assert res["status"] == "added"
    assert calls["add"][0]["sl_price"] == pytest.approx(103.0)
    assert calls["add"][0]["tp_price"] == pytest.approx(130.0)


def test_add_in_opposite_direction_is_refused(env):
    eng, calls = env
    res = _run(eng, _signal("add", side="short"))
    assert res["status"] == "skipped"
    assert "相反" in res["reason"]
    assert calls["add"] == []


def test_add_not_blocked_by_recent_open_lock(env):
    """开仓成功后会留下 90 秒的标的忙碌标记，但加仓不能被它挡掉。

    频道的"开仓 / 现价加仓"经常前后脚发出，用那把锁会把正常加仓全部吃掉。
    """
    eng, calls = env
    assert eng._symbol_acquire(SYMBOL) is True  # 模拟刚刚开过仓
    res = _run(eng, _signal("add"))
    assert res["status"] == "added"
    assert len(calls["add"]) == 1


@pytest.mark.parametrize("event", ["close", "reduce", "move_stop", "add"])
def test_no_local_position_never_places_orders(env, event):
    """没有持仓时，四类事件都必须是"跳过"，不能凭空下单。"""
    eng, calls = env
    with session_scope() as s:
        s.query(PositionRecord).filter(PositionRecord.symbol == SYMBOL).delete()
    res = _run(eng, _signal(event, reduce_pct=50, sl=104.0))
    assert res["status"] == "skipped"
    assert "持仓" in res["reason"]
    assert calls["add"] == [] and calls["close"] == [] and calls["amend"] == []


def test_dry_run_records_without_touching_exchange(env):
    eng, calls = env
    update_group("signal_follow", {"dry_run": True})
    res = _run(eng, _signal("close"))
    assert res["status"] == "dry_run"
    assert calls["close"] == []


# ------------------------------------------------------------------ broker 合并

def test_add_to_position_recomputes_weighted_average(monkeypatch):
    """加仓必须把数量累加、把均价重算，而不是整行覆盖成只剩新加的这笔。"""
    from nanoquant.exchange import broker as broker_mod
    from nanoquant.exchange.precision import SymbolFilters

    sym = "MERGEUSDT"
    init_db()
    with session_scope() as s:
        s.query(PositionRecord).filter(PositionRecord.symbol == sym).delete()
        s.add(
            PositionRecord(
                symbol=sym, side="long", qty=2.0, entry_price=100.0, mark_price=105.0,
                leverage=10.0, margin=20.0, sl_price=95.0, tp_price=120.0,
                peak_price=105.0, opened_at=int(time.time()), mode="demo", status="open",
            )
        )
    with session_scope() as s:
        pos = s.query(PositionRecord).filter(PositionRecord.symbol == sym).first()
        s.expunge(pos)

    placed: list[dict] = []

    async def place_order(**kw):
        placed.append(kw)
        return {
            "executedQty": "1", "avgPrice": "130", "status": "FILLED",
            "orderId": 1, "clientOrderId": kw.get("newClientOrderId"),
        }

    async def noop(*_a, **_kw):
        return {}

    async def ensure_mode():
        return False  # 单向持仓

    async def ensure_symbol(*_a, **_kw):
        return None

    b = broker_mod.BinanceBroker()
    monkeypatch.setattr(b, "_ensure_trade_mode", ensure_mode)
    monkeypatch.setattr(b, "_ensure_symbol", ensure_symbol)
    monkeypatch.setattr(broker_mod.binance_client.client, "place_order", place_order)
    monkeypatch.setattr(broker_mod.binance_client.client, "cancel_all", noop)
    monkeypatch.setattr(broker_mod.binance_client.client, "cancel_all_algo", noop)
    monkeypatch.setattr(broker_mod.binance_client.client, "place_algo_order", noop)

    filters = SymbolFilters(
        symbol=sym, price_tick=0.01, qty_step=0.001, min_qty=0.001, min_notional=5.0
    )
    try:
        res = asyncio.run(
            b.add_to_position(
                position=pos, qty=1.0, filters=filters, sl_price=104.0, tp_price=140.0
            )
        )
        assert res.ok, res.message
        assert placed and float(placed[0]["quantity"]) == 1.0
        with session_scope() as s:
            row = s.query(PositionRecord).filter(PositionRecord.symbol == sym).first()
            assert row.qty == pytest.approx(3.0)  # 2 + 1
            assert row.entry_price == pytest.approx((2 * 100.0 + 1 * 130.0) / 3)
            assert row.sl_price == pytest.approx(104.0)
            assert row.tp_price == pytest.approx(140.0)
            assert row.margin == pytest.approx(row.qty * row.entry_price / 10.0)
    finally:
        with session_scope() as s:
            s.query(PositionRecord).filter(PositionRecord.symbol == sym).delete()

