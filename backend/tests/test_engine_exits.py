"""持仓管理（worker._manage_positions）：止损、保本上移、部分止盈.

这里是历史上真出过钱的位置：移动止损 24 笔全亏、保本上移把止盈单撤掉不再补、
部分止盈算错数量导致平仓被拒。以前这些分支没有测试，只能靠"跑一段看看"。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.exchange.broker import ExecResult
from nanoquant.models import PositionRecord
from nanoquant.services.settings import get_engine, get_strategy, update_group


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _position(symbol: str, *, side="long", entry=100.0, qty=1.0, sl=99.0, tp=103.0,
              initial_sl=99.0, partial_at=0, score=0.5) -> int:
    with session_scope() as s:
        row = PositionRecord(
            symbol=symbol, side=side, qty=qty, entry_price=entry, mark_price=entry,
            leverage=10.0, margin=qty * entry / 10.0, status="open", mode="demo",
            sl_price=sl, tp_price=tp, initial_sl_price=initial_sl, peak_price=entry,
            partial_taken_at=partial_at, opened_at=int(time.time()) - 60,
            signal_score=score, features={"regime": "trend"},
        )
        s.add(row)
        s.flush()
        return row.id


def _broker(price: float, *, ok: bool = True):
    class _B:
        mode = "demo"

        def __init__(self) -> None:
            self.closed: list[dict] = []
            self.amended: list[dict] = []

        async def close_position(self, *, position, reason, qty=None):
            self.closed.append({"symbol": position.symbol, "reason": reason, "qty": qty})
            return ExecResult(ok, price=price, qty=qty or position.qty, message="" if ok else "拒绝")

        async def amend_stop(self, symbol, side, stop, qty, tp_price=0.0):
            self.amended.append(
                {"symbol": symbol, "side": side, "stop": stop, "qty": qty, "tp": tp_price}
            )
            return ExecResult(True, price=stop)

    return _B()


def _run(eng, broker, price: float, cfg=None):
    async def _tickers(_symbols):
        return {p.symbol: SimpleNamespace(last=price, quote_volume=1e8) for p in _open_rows()}

    worker_mod.binance_client.tickers = _tickers  # type: ignore[assignment]
    cfg = cfg or get_strategy()
    asyncio.run(eng._manage_positions(broker, cfg, get_engine()))


def _open_rows():
    with session_scope() as s:
        return s.query(PositionRecord).filter(PositionRecord.status == "open").all()


@pytest.fixture(autouse=True)
def _purge_exit_positions():
    """每个用例结束后清掉 EXIT* 仓位：断言失败时也能清，避免残留仓位污染后续用例."""
    yield
    with session_scope() as s:
        s.query(PositionRecord).filter(PositionRecord.symbol.like("EXIT%")).delete(synchronize_session=False)


@pytest.fixture(autouse=True)
def _restore_tickers():
    original = worker_mod.binance_client.tickers
    yield
    worker_mod.binance_client.tickers = original  # type: ignore[assignment]


def test_stop_loss_breach_closes_position():
    pid = _position("EXITSLUSDT", entry=100.0, sl=99.0, tp=103.0)
    eng = worker_mod.TradingEngine()
    closed: list[tuple[int, str]] = []

    async def _close(_broker, position_id, reason, mark):
        closed.append((position_id, reason))

    eng._close = _close
    _run(eng, _broker(98.9), 98.9)

    assert closed and closed[0][0] == pid
    assert closed[0][1] in ("stop_loss", "trailing_stop")
    _cleanup(pid)


def test_take_profit_breach_closes_position():
    pid = _position("EXITTPUSDT", entry=100.0, sl=99.0, tp=103.0)
    eng = worker_mod.TradingEngine()
    closed: list[str] = []

    async def _close(_broker, position_id, reason, mark):
        closed.append(reason)

    eng._close = _close
    # 关掉部分止盈，否则 3.5R 会先走 partial_tp 分支（那是另一条用例）
    prev = get_strategy().partial_tp_at_r
    update_group("strategy", {"partial_tp_at_r": 0.0})
    try:
        _run(eng, _broker(103.5), 103.5)
    finally:
        update_group("strategy", {"partial_tp_at_r": prev})

    assert closed and closed[0] in ("take_profit", "trailing_stop")
    _cleanup(pid)


def test_breakeven_move_keeps_take_profit(monkeypatch):
    """保本上移必须把止盈一起重挂：amend_stop 内部 cancel_all，漏传就等于删掉止盈.

    用 +0.9%：+0.8% 会被浮点算成 0.7999…，而 +1% 以上会先被 ROI 阶梯平掉。
    """
    pid = _position("EXITBEUSDT", entry=100.0, sl=99.0, tp=103.0)
    eng = worker_mod.TradingEngine()
    broker = _broker(100.9)
    cfg_now = get_strategy()
    prev_be, prev_pt = cfg_now.breakeven_at_r, cfg_now.partial_tp_at_r
    update_group("strategy", {"breakeven_at_r": 0.8, "partial_tp_at_r": 0.0})
    try:
        _run(eng, broker, 100.9)
    finally:
        update_group("strategy", {"breakeven_at_r": prev_be, "partial_tp_at_r": prev_pt})

    assert broker.amended, "浮盈 1R 应该把止损推到保本"
    call = broker.amended[0]
    assert call["stop"] > 100.0
    assert call["tp"] == pytest.approx(103.0), "止盈必须原样带回去"
    assert not broker.closed
    _cleanup(pid)


def test_partial_take_profit_marks_flag_once():
    pid = _position("EXITPARTUSDT", entry=100.0, sl=99.0, initial_sl=99.0, tp=105.0)
    eng = worker_mod.TradingEngine()
    broker = _broker(101.4)

    async def _close(*_a, **_k):  # 部分止盈后本轮 continue，不该再触发整仓平仓
        raise AssertionError("部分止盈后不应再走 _close")

    eng._close = _close
    cfg_now = get_strategy()
    prev_at, prev_ratio = cfg_now.partial_tp_at_r, cfg_now.partial_tp_ratio
    update_group("strategy", {"partial_tp_at_r": 1.0, "partial_tp_ratio": 0.5})
    try:
        _run(eng, broker, 101.4)
    finally:
        update_group("strategy", {"partial_tp_at_r": prev_at, "partial_tp_ratio": prev_ratio})

    assert broker.closed and broker.closed[0]["reason"] == "partial_tp"
    assert 0 < broker.closed[0]["qty"] < 1.0
    with session_scope() as s:
        assert s.get(PositionRecord, pid).partial_taken_at > 0

    # 第二轮不应再部分止盈一次（后续正常出场是允许的，这里只统计 partial_tp）
    broker2 = _broker(101.4)
    closed2: list[str] = []

    async def _close2(_broker, position_id, reason, mark):
        closed2.append(reason)

    eng._close = _close2
    _run(eng, broker2, 101.4)
    assert [c for c in broker2.closed if c["reason"] == "partial_tp"] == []
    assert "partial_tp" not in closed2
    _cleanup(pid)


def test_market_data_failure_skips_cycle_without_crashing():
    pid = _position("EXITDOWNUSDT")

    async def _boom(_symbols):
        raise worker_mod.BinanceError(-1001, "连接断开")

    worker_mod.binance_client.tickers = _boom  # type: ignore[assignment]
    eng = worker_mod.TradingEngine()

    asyncio.run(eng._manage_positions(_broker(100.0), get_strategy(), get_engine()))  # 不抛即通过

    with session_scope() as s:
        assert s.get(PositionRecord, pid).status == "open", "取不到价格时什么都不能做"
    _cleanup(pid)


def _cleanup(pid: int):
    with session_scope() as s:
        row = s.get(PositionRecord, pid)
        if row is not None:
            s.delete(row)
