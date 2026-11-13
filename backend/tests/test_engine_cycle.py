"""引擎主循环与交易所对账（worker.cycle / _reconcile_live）.

这两段以前是 0 覆盖，但它们是"钱会不会莫名其妙消失"的位置：
- cycle 决定每一轮到底走不走、熔断写不写、要不要在熔断时平仓；
- _reconcile_live 决定本地账本与交易所谁说了算（幽灵仓/漏记仓）。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import risk as risk_mod
from nanoquant.engine import worker as worker_mod
from nanoquant.models import EngineEvent, PositionRecord
from nanoquant.services import state
from nanoquant.services.settings import update_group

MODE = "demo"


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


class _Broker:
    mode = MODE

    def __init__(self, *, equity: float = 100.0, venue=None, fail: bool = False) -> None:
        self.equity = equity
        self._venue = venue or []
        self._fail = fail
        self.closed: list[tuple[str, str]] = []

    async def account(self):
        if self._fail:
            raise worker_mod.BinanceError(-1021, "Timestamp outside recvWindow")
        return SimpleNamespace(
            equity=self.equity,
            wallet=self.equity,
            available=self.equity,
            unrealized=0.0,
            margin_used=0.0,
            as_dict=lambda: {"equity": self.equity},
        )

    async def positions(self):
        if self._fail:
            raise worker_mod.BinanceError(-1001, "内部错误")
        return list(self._venue)


def _engine(*, stub_reconcile: bool = True, **stubs):
    """造一个只保留被测方法的引擎；其余阶段全部桩化.

    ``stub_reconcile=False`` 用于测对账本身，否则会把 `_reconcile_live` 也桩掉（第一版就踩了这个坑）。
    """
    eng = worker_mod.TradingEngine()

    async def _noop(*_a, **_k):
        return None

    for name in (
        "_manage_positions",
        "_check_liquidation_risk",
        "_health_check",
        "_sample_equity",
        "_check_trade_mode",
        "_scan",
    ):
        setattr(eng, name, stubs.get(name, _noop))
    if stub_reconcile:
        eng._reconcile_live = _noop
    return eng


@pytest.fixture()
def clean_state():
    state.set_many({"halted": "", "last_error": "", "day_anchor": "", "day_start_equity": "", "equity_peak": ""})
    yield
    state.set_many({"halted": "", "last_error": ""})


def test_cycle_returns_early_when_account_unavailable(monkeypatch, clean_state):
    eng = _engine()
    broker = _Broker(fail=True)
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)

    out = asyncio.run(eng.cycle())

    assert "账户不可用" in out["skipped"]
    assert "账户同步失败" in state.get("last_error")


def test_cycle_skips_outside_trading_window(monkeypatch, clean_state):
    eng = _engine()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(worker_mod.risk_mod, "trading_window_open", lambda cfg: (False, "当前不在交易时段"))

    out = asyncio.run(eng.cycle())

    assert out["skipped"] == "当前不在交易时段"


def test_cycle_records_halt_and_skips_scan(monkeypatch, clean_state):
    """闸门拦截时必须写 halted 并跳过扫描，否则熔断只是"看不见"."""
    eng = _engine()
    scanned = []

    async def _scan(*_a, **_k):
        scanned.append(1)
        return []

    eng._scan = _scan
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(worker_mod.risk_mod, "trading_window_open", lambda cfg: (True, ""))
    monkeypatch.setattr(
        worker_mod.risk_mod,
        "global_gates",
        lambda account, ec, rc: risk_mod.GateDecision(False, "触发最大回撤", halt="最大回撤 60% 超过上限 50%"),
    )
    update_group("engine", {"close_on_halt": False})

    out = asyncio.run(eng.cycle())

    assert out["skipped"] == "触发最大回撤"
    assert "最大回撤" in state.get("halted")
    assert not scanned, "被闸门拦住就不该再扫行情（省权重，也避免半截下单）"


def test_cycle_flattens_positions_on_halt_when_enabled(monkeypatch, clean_state):
    eng = _engine()
    flattened: list[str] = []

    async def _flat(reason: str) -> None:
        flattened.append(reason)

    eng._flatten_on_halt = _flat
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(worker_mod.risk_mod, "trading_window_open", lambda cfg: (True, ""))
    monkeypatch.setattr(
        worker_mod.risk_mod,
        "global_gates",
        lambda account, ec, rc: risk_mod.GateDecision(False, "日亏超限", halt="日内亏损 -30% 超过上限 25%"),
    )
    old = update_group("engine", {"close_on_halt": True})
    try:
        asyncio.run(eng.cycle())
    finally:
        update_group("engine", {"close_on_halt": False})

    assert flattened == ["日内亏损 -30% 超过上限 25%"]
    update_group("engine", {"close_on_halt": old["close_on_halt"]})


def test_cycle_happy_path_clears_halt(monkeypatch, clean_state):
    eng = _engine()
    signals = [SimpleNamespace(symbol="AAAUSDT"), SimpleNamespace(symbol="BBBUSDT")]

    async def _scan(*_a, **_k):
        return signals

    eng._scan = _scan
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker(equity=123.456))
    monkeypatch.setattr(worker_mod.risk_mod, "trading_window_open", lambda cfg: (True, ""))
    monkeypatch.setattr(
        worker_mod.risk_mod, "global_gates", lambda account, ec, rc: risk_mod.GateDecision(True, "")
    )
    state.set("halted", "日内亏损 -30% 超过上限 25%")

    out = asyncio.run(eng.cycle())

    assert out == {"signals": 2, "equity": 123.456}
    assert state.get("halted") == "", "闸门放行说明熔断已解除，必须把 halted 清掉"


# ------------------------------------------------------------ 对账
def _venue(symbol: str, side="long", qty=1.0, entry=100.0, mark=101.0, lev=14.0, upnl=1.0):
    return SimpleNamespace(
        symbol=symbol, side=side, qty=qty, entry_price=entry, mark_price=mark,
        leverage=lev, unrealized=upnl,
    )


def _open_position(symbol: str, side="long", qty=1.0, entry=100.0) -> int:
    with session_scope() as s:
        row = PositionRecord(
            symbol=symbol, side=side, qty=qty, entry_price=entry, mark_price=entry,
            leverage=14.0, margin=qty * entry / 14.0, status="open", mode=MODE,
            opened_at=int(time.time()), sl_price=0.0, tp_price=0.0, peak_price=entry,
        )
        s.add(row)
        s.flush()
        return row.id


def test_reconcile_closes_local_position_missing_on_venue(monkeypatch, clean_state):
    """交易所已经没仓了，本地还挂 open —— 必须按 venue_sync 关掉，否则会挡住新开仓."""
    pid = _open_position("GHOSTUSDT")
    eng = _engine(stub_reconcile=False)
    broker = _Broker(venue=[])

    asyncio.run(eng._reconcile_live(broker))

    with session_scope() as s:
        row = s.get(PositionRecord, pid)
        assert row.status == "closed"
        assert row.exit_reason == "venue_sync"
        s.delete(row)


def test_reconcile_adopts_external_position(monkeypatch, clean_state):
    """手动开的仓必须被接管进账本，否则引擎永远不知道它存在."""
    adopted: list[tuple] = []

    def _fake_record(symbol, side, qty, entry, mark, lev, mode, **kw):
        adopted.append((symbol, side, qty, kw.get("sl_price"), kw.get("tp_price")))

    async def _algo(_symbol):
        return 0.0, 0.0

    eng = _engine(stub_reconcile=False)
    eng._algo_sl_tp = _algo
    monkeypatch.setattr(worker_mod, "record_external_position", _fake_record)
    broker = _Broker(venue=[_venue("MANUALUSDT", side="short", qty=3.0)])

    asyncio.run(eng._reconcile_live(broker))

    assert adopted == [("MANUALUSDT", "short", 3.0, 0.0, 0.0)]
    with session_scope() as s:
        ev = (
            s.query(EngineEvent)
            .filter(EngineEvent.message.like("%MANUALUSDT%"))
            .order_by(EngineEvent.ts.desc())
            .first()
        )
        assert ev is not None and "外部仓位" in ev.message


def test_reconcile_updates_qty_and_mark_for_known_position(clean_state):
    pid = _open_position("SYNCUSDT", qty=1.0, entry=100.0)
    eng = _engine(stub_reconcile=False)
    broker = _Broker(venue=[_venue("SYNCUSDT", qty=2.5, mark=103.5, upnl=8.75)])

    asyncio.run(eng._reconcile_live(broker))

    with session_scope() as s:
        row = s.get(PositionRecord, pid)
        assert row.qty == pytest.approx(2.5), "部分成交/手工加减仓后本地数量必须跟上交易所"
        assert row.mark_price == pytest.approx(103.5)
        assert row.venue_unrealized == pytest.approx(8.75)
        s.delete(row)


def test_reconcile_survives_venue_error(clean_state):
    """取不到交易所仓位时只能告警，不能把整轮 cycle 打断."""
    eng = _engine(stub_reconcile=False)
    broker = _Broker(fail=True)

    asyncio.run(eng._reconcile_live(broker))  # 不抛异常即通过

    with session_scope() as s:
        ev = (
            s.query(EngineEvent)
            .filter(EngineEvent.message.like("%仓位同步失败%"))
            .order_by(EngineEvent.ts.desc())
            .first()
        )
        assert ev is not None
