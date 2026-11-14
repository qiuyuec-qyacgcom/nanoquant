"""复盘调度与交易模式体检.

复盘这条链负责"把交易结果变成心法"，它坏掉不会立刻亏钱，但会让系统永远学不到东西；
交易模式体检则决定面板上的告警是否与账户真实状态一致。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.models import EngineEvent, TradeRecord
from nanoquant.services import state
from nanoquant.services.settings import get_evolution, update_group


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


@pytest.fixture(autouse=True)
def _clean():
    prev = get_evolution()
    yield
    update_group("evolution", prev.model_dump())
    with session_scope() as s:
        s.query(TradeRecord).filter(TradeRecord.symbol.like("REV%")).delete(synchronize_session=False)


def _events_like(pattern: str) -> int:
    with session_scope() as s:
        return s.query(EngineEvent).filter(EngineEvent.message.like(f"%{pattern}%")).count()


def _trade(reviewed: bool = False) -> int:
    with session_scope() as s:
        row = TradeRecord(
            symbol="REVTESTUSDT", side="long", qty=1.0, entry_price=100.0, exit_price=101.0,
            pnl=1.0, pnl_pct=1.0, roi_pct=10.0, fees=0.1, mode="demo",
            opened_at=int(time.time()) - 600, closed_at=int(time.time()),
            exit_reason="take_profit", reviewed=reviewed,
        )
        s.add(row)
        s.flush()
        return row.id


# ------------------------------------------------------------ 单笔复盘
def test_review_trade_records_event(monkeypatch):
    calls: list[int] = []

    async def _review(trade_id):
        calls.append(trade_id)
        return {"ok": True, "skipped": False, "sample_size": 5, "change_status": "applied", "applied": [{"id": 1}]}

    monkeypatch.setattr(worker_mod.evolution_review, "review_trade", _review)
    eng = worker_mod.TradingEngine()

    before = _events_like("批量复盘完成")
    asyncio.run(eng._review_trade(1))

    assert calls == [1]
    assert _events_like("批量复盘完成") - before == 1


def test_review_trade_dedupes_inflight_only(monkeypatch):
    """平仓触发与补扫可能同时命中同一笔：在途时必须挡住，串行再触发是允许的（由 reviewed 标记兜底）."""
    started = asyncio.Event()
    release = asyncio.Event()
    calls: list[int] = []

    async def _review(trade_id):
        calls.append(trade_id)
        started.set()
        await release.wait()
        return {"ok": True, "skipped": False}

    monkeypatch.setattr(worker_mod.evolution_review, "review_trade", _review)
    eng = worker_mod.TradingEngine()

    async def scenario():
        first = asyncio.create_task(eng._review_trade(9))
        await started.wait()
        await eng._review_trade(9)  # 在途 → 直接返回
        release.set()
        await first

    asyncio.run(scenario())

    assert calls == [9], "在途去重失效会重复消耗 AI 额度"


def test_review_trade_failure_is_logged_not_raised(monkeypatch):
    async def _boom(_trade_id):
        raise RuntimeError("模型超时")

    monkeypatch.setattr(worker_mod.evolution_review, "review_trade", _boom)
    eng = worker_mod.TradingEngine()

    before = _events_like("复盘失败")
    asyncio.run(eng._review_trade(2))

    assert _events_like("复盘失败") - before == 1
    assert 2 not in eng._reviewing, "失败后必须释放去重标记，否则这笔再也复盘不了"


# ------------------------------------------------------------ 补扫
def test_backfill_runs_batch_only_when_enough_samples(monkeypatch):
    update_group("evolution", {"enabled": True, "review_on_close": True, "batch_review_size": 3})
    eng = worker_mod.TradingEngine()
    eng._last_review_backfill = 0
    calls: list[int] = []

    async def _batch():
        calls.append(1)
        return {"ok": True, "skipped": False, "sample_size": 3, "change_status": "applied"}

    monkeypatch.setattr(worker_mod.evolution_review, "review_batch", _batch)

    # 样本不足时不该调模型
    _trade()
    asyncio.run(eng._review_backfill_async())
    assert calls == []

    for _ in range(3):
        _trade()
    asyncio.run(eng._review_backfill_async())
    assert calls == [1]


def test_backfill_respects_switch_and_interval(monkeypatch):
    eng = worker_mod.TradingEngine()
    calls: list[int] = []

    async def _batch():
        calls.append(1)
        return {"ok": True}

    monkeypatch.setattr(worker_mod.evolution_review, "review_batch", _batch)
    update_group("evolution", {"enabled": False})
    eng._last_review_backfill = 0

    asyncio.run(eng._review_backfill_async())
    assert calls == [], "开关关闭时补扫不该有任何模型调用"


# ------------------------------------------------------------ 全局复盘
def test_global_review_waits_for_enough_trades(monkeypatch):
    update_group("evolution", {"enabled": True, "global_review_every_trades": 5})
    eng = worker_mod.TradingEngine()
    calls: list[int] = []

    async def _global():
        calls.append(1)
        return {"applied": [], "retired": []}

    monkeypatch.setattr(worker_mod.evolution_review, "global_review", _global)

    eng._trades_since_review = 2
    asyncio.run(eng._maybe_global_review())
    assert calls == [] and eng._trades_since_review == 2

    eng._trades_since_review = 5
    asyncio.run(eng._maybe_global_review())
    assert calls == [1]
    assert eng._trades_since_review == 0, "触发后要清零，否则每轮都会复盘一次"


def test_global_review_failure_does_not_count_as_success(monkeypatch):
    update_group("evolution", {"enabled": True, "global_review_every_trades": 2})
    eng = worker_mod.TradingEngine()
    eng._trades_since_review = 2

    async def _boom():
        raise RuntimeError("模型 500")

    monkeypatch.setattr(worker_mod.evolution_review, "global_review", _boom)

    asyncio.run(eng._maybe_global_review())  # 不抛异常即通过


# ------------------------------------------------------------ 交易模式体检
class _ModeBroker:
    def __init__(self, status: dict | None = None, fail: bool = False) -> None:
        self.status = status or {}
        self.fail = fail

    async def trade_mode_status(self, symbols=None):
        if self.fail:
            raise worker_mod.BinanceError(-1001, "内部错误")
        return self.status


def test_trade_mode_warns_on_single_side_account(monkeypatch):
    state.set("trade_mode", "")
    eng = worker_mod.TradingEngine()
    eng._trade_mode_ts = 0
    eng._trade_mode_warn_ts = 0
    broker = _ModeBroker({"hedge_mode": False, "need_hedge": True, "need_isolated": []})

    before = _events_like("账户为单向持仓")
    asyncio.run(eng._check_trade_mode(broker))
    asyncio.run(eng._check_trade_mode(broker))  # 5 分钟内不重复查

    assert _events_like("账户为单向持仓") - before == 1
    assert "hedge_mode" in state.get("trade_mode")


def test_trade_mode_warns_on_cross_margin_symbols(monkeypatch):
    eng = worker_mod.TradingEngine()
    eng._trade_mode_ts = 0
    eng._trade_mode_warn_ts = 0
    broker = _ModeBroker({"hedge_mode": True, "need_hedge": False, "need_isolated": ["AAAUSDT", "BBBUSDT"]})

    before = _events_like("仍为全仓")
    asyncio.run(eng._check_trade_mode(broker))

    assert _events_like("仍为全仓") - before == 1


def test_trade_mode_silent_when_already_correct():
    eng = worker_mod.TradingEngine()
    eng._trade_mode_ts = 0
    eng._trade_mode_warn_ts = 0
    broker = _ModeBroker({"hedge_mode": True, "need_hedge": False, "need_isolated": []})

    before = _events_like("单向持仓")
    asyncio.run(eng._check_trade_mode(broker))

    assert _events_like("单向持仓") == before


def test_trade_mode_survives_broker_error():
    eng = worker_mod.TradingEngine()
    eng._trade_mode_ts = 0

    asyncio.run(eng._check_trade_mode(_ModeBroker(fail=True)))  # 不抛即通过
