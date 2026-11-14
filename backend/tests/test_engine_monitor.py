"""引擎监控三件套：强平预警、健康检查、权益采样.

这些不是交易逻辑，但决定了"出事时你有没有被通知"：
20x 仓位爆仓前没人告警、扫描卡死半小时没人知道、权益曲线断点导致回撤基准算错。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.models import EngineEvent, EquityPoint, Signal
from nanoquant.services import state


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _pos(symbol: str, *, mark: float, liq: float, side: str = "long"):
    return SimpleNamespace(symbol=symbol, side=side, mark_price=mark, liquidation_price=liq, qty=1.0)


class _Broker:
    def __init__(self, positions=None, explode: bool = False) -> None:
        self._positions = positions or []
        self.explode = explode

    async def positions(self):
        if self.explode:
            raise worker_mod.BinanceError(-1001, "内部错误")
        return self._positions


def _events_like(pattern: str) -> int:
    with session_scope() as s:
        return (
            s.query(EngineEvent)
            .filter(EngineEvent.message.like(f"%{pattern}%"))
            .count()
        )


# ------------------------------------------------------------ 强平预警
def test_liquidation_warning_fires_and_dedupes():
    eng = worker_mod.TradingEngine()
    cfg = SimpleNamespace(liquidation_warn_pct=20.0)
    broker = _Broker([_pos("LIQNEARUSDT", mark=100.0, liq=85.0)])  # 距强平 15%

    before = _events_like("距强平价")
    asyncio.run(eng._check_liquidation_risk(broker, cfg))
    asyncio.run(eng._check_liquidation_risk(broker, cfg))  # 5 分钟内不重复告警
    after = _events_like("距强平价")

    assert after - before == 1, "同一次危险只该告警一次，否则每轮刷屏"


def test_liquidation_warning_ignores_safe_positions():
    eng = worker_mod.TradingEngine()
    cfg = SimpleNamespace(liquidation_warn_pct=20.0)
    broker = _Broker([_pos("LIQFARUSDT", mark=100.0, liq=60.0)])  # 距强平 40%

    before = _events_like("LIQFARUSDT")
    asyncio.run(eng._check_liquidation_risk(broker, cfg))

    assert _events_like("LIQFARUSDT") == before


def test_liquidation_check_can_be_disabled():
    eng = worker_mod.TradingEngine()
    called: list[int] = []

    class _B:
        async def positions(self):
            called.append(1)
            return []

    asyncio.run(eng._check_liquidation_risk(_B(), SimpleNamespace(liquidation_warn_pct=0.0)))

    assert called == [], "阈值 0 = 关闭，连行情都不该拉"


def test_liquidation_check_survives_venue_error():
    eng = worker_mod.TradingEngine()
    asyncio.run(eng._check_liquidation_risk(_Broker(explode=True), SimpleNamespace(liquidation_warn_pct=20.0)))


# ------------------------------------------------------------ 健康检查
def test_health_check_warns_when_signals_stall():
    # 健康检查看的是"最新的那条信号有多老"，所以要先清空别的用例留下的新信号
    with session_scope() as s:
        s.query(Signal).delete(synchronize_session=False)
        s.add(Signal(symbol="STALLUSDT", ts=int(time.time()) - 600, direction="long", score=0.5,
                     decision="open", mode="demo"))
    eng = worker_mod.TradingEngine()
    eng._last_health_check = 0

    before = _events_like("扫描可能停滞")
    asyncio.run(eng._health_check())

    assert _events_like("扫描可能停滞") - before == 1


def test_health_check_is_throttled():
    eng = worker_mod.TradingEngine()
    eng._last_health_check = time.time()

    asyncio.run(eng._health_check())  # 60s 内直接返回

    assert eng._last_health_check > 0


# ------------------------------------------------------------ 权益采样
def test_equity_sampling_is_throttled_to_one_per_minute():
    eng = worker_mod.TradingEngine()
    # 把其它周期性任务推迟，避免它们创建后台任务
    now = int(time.time())
    eng._last_cleanup = now
    eng._last_dlq = now
    eng._last_digest = now
    eng._last_review_backfill = now
    account = SimpleNamespace(wallet=100.0, available=90.0, unrealized=2.0, margin_used=10.0)

    state.set("last_equity_sample", "0")
    with session_scope() as s:
        s.query(EquityPoint).delete(synchronize_session=False)
    asyncio.run(eng._sample_equity(account))
    asyncio.run(eng._sample_equity(account))  # 60s 内第二次应被跳过

    with session_scope() as s:
        assert s.query(EquityPoint).count() == 1
        point = s.query(EquityPoint).first()
        assert point.wallet == pytest.approx(100.0)
        assert point.unrealized == pytest.approx(2.0)
        s.query(EquityPoint).delete(synchronize_session=False)
    state.set("last_equity_sample", "0")


# ------------------------------------------------------------ 运行态面板
class _StatusBroker:
    mode = "demo"

    def __init__(self, *, equity: float = 120.0, venue=None, fail: bool = False) -> None:
        self.equity = equity
        self._venue = venue or []
        self.fail = fail

    async def account(self):
        if self.fail:
            raise worker_mod.BinanceError(-1021, "Timestamp outside recvWindow")
        return SimpleNamespace(
            equity=self.equity, wallet=self.equity, available=self.equity - 20,
            unrealized=0.5, margin_used=20.0,
            as_dict=lambda: {
                "equity": self.equity, "wallet": self.equity,
                "available": self.equity - 20, "unrealized": 0.5, "margin_used": 20.0,
            },
        )

    async def positions(self):
        return list(self._venue)


def _open_status_position(symbol: str = "STATUSUSDT") -> int:
    from nanoquant.models import PositionRecord

    with session_scope() as s:
        row = PositionRecord(
            symbol=symbol, side="long", qty=1.0, entry_price=100.0, mark_price=101.0,
            leverage=10.0, margin=10.0, status="open", mode="demo",
            sl_price=98.0, tp_price=104.0, initial_sl_price=98.0, peak_price=101.0,
            opened_at=int(time.time()) - 120, features={},
        )
        s.add(row)
        s.flush()
        return row.id


def test_status_returns_positions_and_account(monkeypatch):
    pid = _open_status_position()
    eng = worker_mod.TradingEngine()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _StatusBroker())
    try:
        out = asyncio.run(eng.status())
    finally:
        from nanoquant.models import PositionRecord

        with session_scope() as s:
            row = s.get(PositionRecord, pid)
            if row:
                s.delete(row)

    assert out["account"]["equity"] == pytest.approx(120.0)
    row = next(p for p in out["positions"] if p["symbol"] == "STATUSUSDT")
    assert row["sl_price"] == pytest.approx(98.0)
    assert row["tp_price"] == pytest.approx(104.0)
    assert out["mode"] == "demo"
    assert "stats" in out and "positions" in out


def test_status_keeps_last_account_when_exchange_fails(monkeypatch):
    """交易所抖一下不能把面板刷成 0：保留上一轮数值并标 stale."""
    eng = worker_mod.TradingEngine()
    eng._last_account = {"equity": 88.0, "wallet": 88.0, "available": 70.0, "unrealized": 0.0, "margin_used": 18.0}
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _StatusBroker(fail=True))

    out = asyncio.run(eng.status())

    assert out["account"]["equity"] == pytest.approx(88.0)
    assert out["account"]["stale"] is True
    assert "error" in out["account"]


def test_status_merges_venue_quantity(monkeypatch):
    """面板的浮盈亏/名义额必须取同一次请求的交易所值，而不是上一轮 DB 快照."""
    pid = _open_status_position("STATUSMIXUSDT")
    venue = SimpleNamespace(
        symbol="STATUSMIXUSDT", side="long", qty=2.0, entry_price=100.0, mark_price=101.0,
        leverage=10.0, unrealized=2.0,
    )
    eng = worker_mod.TradingEngine()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _StatusBroker(venue=[venue]))
    try:
        out = asyncio.run(eng.status())
    finally:
        from nanoquant.models import PositionRecord

        with session_scope() as s:
            row = s.get(PositionRecord, pid)
            if row:
                s.delete(row)

    row = next(p for p in out["positions"] if p["symbol"] == "STATUSMIXUSDT")
    # 合并只覆盖"价格类"字段：数量与成本归本地账本（由对账写回），浮盈亏以交易所为准
    assert row["unrealized"] == pytest.approx(2.0)
    assert row["mark_price"] == pytest.approx(101.0)
    assert row["unrealized_source"] == "exchange"
