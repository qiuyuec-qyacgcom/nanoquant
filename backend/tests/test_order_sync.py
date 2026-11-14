"""委托状态收敛（services/order_sync.py）.

订单表原来只记录下单那一瞬的回执：市价单一般是 FILLED，但挂单回执永远是 NEW，
之后成交了还是撤了、成交了多少，系统一无所知。面板上会永远挂着一条 NEW、
成交量停在 0；更麻烦的是"入场单一张都没成交"时，账本里那条仓位其实是幽灵仓。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.exchange import broker as broker_mod
from nanoquant.exchange import client as client_mod
from nanoquant.exchange.client import BinanceError
from nanoquant.models import EngineEvent, OrderRecord
from nanoquant.services import order_sync

SYMBOL = "ORDSYNCUSDT"


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


@pytest.fixture(autouse=True)
def _clean():
    # 其它用例也会往订单表写东西，这里按整表清理才能保证"待查队列"可预期
    with session_scope() as s:
        s.query(OrderRecord).delete(synchronize_session=False)
    yield
    with session_scope() as s:
        s.query(OrderRecord).delete(synchronize_session=False)


def _order(status: str = "NEW", *, purpose: str = "entry", order_id: str = "123",
           cid: str = "nq-1", ts: int | None = None, mode: str = "demo") -> int:
    with session_scope() as s:
        row = OrderRecord(
            symbol=SYMBOL, side="BUY", order_type="LIMIT", qty=0.0, price=0.0,
            status=status, purpose=purpose, client_order_id=cid,
            exchange_order_id=order_id, mode=mode,
            ts=ts or int(time.time()),
        )
        s.add(row)
        s.flush()
        return row.id


class _Client:
    def __init__(self, resp=None, fail: BaseException | None = None) -> None:
        self.resp = resp or {}
        self.fail = fail
        self.calls: list[tuple] = []

    async def get_order(self, symbol, order_id=None, client_id=None):
        self.calls.append((symbol, order_id, client_id))
        if self.fail is not None:
            raise self.fail
        return self.resp


def _patch(monkeypatch, client: _Client):
    monkeypatch.setattr(client_mod, "client", client)


def _row(pid: int) -> OrderRecord:
    with session_scope() as s:
        row = s.get(OrderRecord, pid)
        s.expunge(row)
        return row


# ------------------------------------------------------------ 待查范围
def test_pending_orders_skips_terminal_and_old():
    open_id = _order("NEW")
    for status in ("FILLED", "CANCELED", "EXPIRED", "REJECTED"):
        _order(status)
    stale = _order("NEW", ts=int(time.time()) - 3 * 86400)

    ids = [item["id"] for item in order_sync.pending_orders(limit=50)]

    assert open_id in ids
    assert stale not in ids, "24 小时前的单子不该再查（白吃权重）"
    assert len(ids) == 1


def test_empty_queue_makes_no_network_call(monkeypatch):
    client = _Client()
    _patch(monkeypatch, client)

    out = asyncio.run(order_sync.sync())

    assert out == {"checked": 0, "updated": 0, "errors": 0, "unfilled": []}
    assert client.calls == [], "没有待查委托时一次请求都不该发"


# ------------------------------------------------------------ 状态收敛
def test_filled_order_is_updated_with_real_qty_and_price(monkeypatch):
    pid = _order("NEW")
    _patch(monkeypatch, _Client({"status": "FILLED", "executedQty": "0.42", "avgPrice": "100.5"}))

    out = asyncio.run(order_sync.sync())

    assert out["checked"] == 1 and out["updated"] == 1
    row = _row(pid)
    assert row.status == "FILLED"
    assert row.qty == pytest.approx(0.42), "成交量必须以交易所为准"
    assert row.price == pytest.approx(100.5)
    assert row.raw["synced_at"] > 0


def test_partially_filled_stays_pending(monkeypatch):
    pid = _order("NEW")
    _patch(monkeypatch, _Client({"status": "PARTIALLY_FILLED", "executedQty": "0.2", "avgPrice": "99.8"}))

    asyncio.run(order_sync.sync())

    assert _row(pid).status == "PARTIALLY_FILLED"
    assert pid in [i["id"] for i in order_sync.pending_orders(limit=10)], "还会继续变化，下一轮仍要查"


def test_canceled_entry_without_fill_is_reported(monkeypatch):
    """入场单撤销且成交量为 0 = 交易所根本没建仓，账本里的仓位是幽灵仓."""
    pid = _order("NEW")
    _patch(monkeypatch, _Client({"status": "CANCELED", "executedQty": "0", "avgPrice": "0"}))

    out = asyncio.run(order_sync.sync())

    assert [i["id"] for i in out["unfilled"]] == [pid]
    with session_scope() as s:
        ev = (
            s.query(EngineEvent)
            .filter(EngineEvent.message.like(f"%{SYMBOL}%入场单%"))
            .order_by(EngineEvent.ts.desc())
            .first()
        )
        assert ev is not None and "成交量为 0" in ev.message
    assert _row(pid).status == "CANCELED"


def test_canceled_exit_order_is_not_flagged(monkeypatch):
    """平仓单没成交不是幽灵仓（仓位还在），不该告警."""
    _order("NEW", purpose="exit", cid="nq-x")
    _patch(monkeypatch, _Client({"status": "EXPIRED", "executedQty": "0"}))

    out = asyncio.run(order_sync.sync())

    assert out["unfilled"] == []


def test_exchange_error_is_counted_not_raised(monkeypatch):
    pid = _order("NEW")
    _patch(monkeypatch, _Client(fail=BinanceError(-2013, "Order does not exist")))

    out = asyncio.run(order_sync.sync())

    assert out["checked"] == 1 and out["errors"] == 1 and out["updated"] == 0
    assert _row(pid).status == "NEW", "查不到就保持原状，绝不能瞎改状态"


def test_missing_exchange_id_falls_back_to_client_id(monkeypatch):
    _order("NEW", order_id="", cid="nq-fallback")
    client = _Client({"status": "FILLED", "executedQty": "1", "avgPrice": "100"})
    _patch(monkeypatch, client)

    asyncio.run(order_sync.sync())

    assert client.calls == [(SYMBOL, None, "nq-fallback")]


# ------------------------------------------------------------ 调度
def test_worker_sync_is_throttled(monkeypatch):
    from nanoquant.engine import worker as worker_mod

    eng = worker_mod.TradingEngine()
    calls: list[int] = []

    async def _sync():
        calls.append(1)
        return {"checked": 1, "updated": 1, "errors": 0, "unfilled": []}

    monkeypatch.setattr(worker_mod.order_sync, "sync", _sync)

    asyncio.run(eng._sync_orders_if_due())
    asyncio.run(eng._sync_orders_if_due())  # 60s 内不重复
    eng._last_order_sync = 0
    asyncio.run(eng._sync_orders_if_due())

    assert len(calls) == 2


def test_worker_swallows_sync_failure(monkeypatch):
    from nanoquant.engine import worker as worker_mod

    eng = worker_mod.TradingEngine()

    async def _boom():
        raise RuntimeError("网络抖动")

    monkeypatch.setattr(worker_mod.order_sync, "sync", _boom)

    asyncio.run(eng._sync_orders_if_due())  # 不抛即通过


def test_pending_orders_is_scoped_to_current_environment(monkeypatch):
    """模拟盘的单不能拿去实盘查：只会得到 Order does not exist，还会污染台账."""
    demo_id = _order("NEW", cid="demo-1")
    live_id = _order("NEW", cid="live-1", mode="live")
    client = _Client({"status": "FILLED", "executedQty": "1", "avgPrice": "100"})
    _patch(monkeypatch, client)

    out = asyncio.run(order_sync.sync(mode="demo"))

    assert [i["id"] for i in order_sync.pending_orders(mode="demo")] == []
    assert out["checked"] == 1
    assert [i["id"] for i in order_sync.pending_orders(mode="live")] == [live_id]
    with session_scope() as s:
        assert s.get(OrderRecord, demo_id).status == "FILLED"
        assert s.get(OrderRecord, live_id).status == "NEW", "另一个环境的单不许被改"
