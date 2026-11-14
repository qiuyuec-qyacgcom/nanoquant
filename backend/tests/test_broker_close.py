"""平仓链（broker.close_position / _persist_close）.

这是全系统最后一道"钱出去"的动作，也是最容易出错的：
- 平仓量没按步进对齐 → -1111；
- 部分止盈把保护单一起撤了 → 剩余仓位裸奔；
- 平仓重复下单（引擎与手动同时点）→ 反向开仓；
- 手续费只算出场不算入场 → 复盘统计系统性高估收益。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.exchange import broker as broker_mod
from nanoquant.exchange.client import BinanceError, NoCredentials
from nanoquant.exchange.precision import SymbolFilters
from nanoquant.models import OrderRecord, PositionRecord, TradeRecord
from nanoquant.services.settings import get_risk, update_group

SYMBOL = "CLOSETESTUSDT"


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _position(*, side: str = "long", qty: float = 2.0, entry: float = 100.0, mark: float = 101.0) -> int:
    with session_scope() as s:
        row = PositionRecord(
            symbol=SYMBOL, side=side, qty=qty, entry_price=entry, mark_price=mark,
            leverage=10.0, margin=qty * entry / 10.0, status="open", mode="demo",
            sl_price=98.0, tp_price=104.0, initial_sl_price=98.0, peak_price=mark,
            opened_at=int(time.time()) - 120, features={},
        )
        s.add(row)
        s.flush()
        return row.id


def _detached(pid: int) -> PositionRecord:
    with session_scope() as s:
        row = s.get(PositionRecord, pid)
        s.expunge(row)
        return row


def _cleanup(pid: int) -> None:
    with session_scope() as s:
        row = s.get(PositionRecord, pid)
        if row is not None:
            s.delete(row)
        s.query(TradeRecord).filter(TradeRecord.symbol == SYMBOL).delete(synchronize_session=False)
        s.query(OrderRecord).filter(OrderRecord.symbol == SYMBOL).delete(synchronize_session=False)
    broker_mod._CLOSING.discard(pid)  # noqa: SLF001


def _payload(**over) -> dict:
    base = {
        "close_qty": 2.0, "avg": 101.0, "reason": "take_profit", "fee": 0.101,
        "pnl": 1.899, "latency_ms": 42, "status": "FILLED",
        "client_order_id": "nx-test", "exchange_order_id": "99", "mode": "demo", "raw": {},
    }
    base.update(over)
    return base


# ------------------------------------------------------------ 落库
def test_persist_close_full_writes_trade_and_closes_position():
    pid = _position(qty=2.0, entry=100.0)
    taker = get_risk().fee_rate_taker

    broker_mod._persist_close(_payload(), pid)  # noqa: SLF001

    with session_scope() as s:
        pos = s.get(PositionRecord, pid)
        assert pos.status == "closed"
        assert pos.exit_price == pytest.approx(101.0)
        assert pos.realized_pnl == pytest.approx(1.899)
        trade = s.query(TradeRecord).filter(TradeRecord.symbol == SYMBOL).one()
        entry_fee = 2.0 * 100.0 * taker
        assert trade.fees == pytest.approx(entry_fee + 0.101), "必须把入场费也算进去"
        assert trade.pnl == pytest.approx(1.899 - entry_fee)
        assert trade.exit_reason == "take_profit"
        assert pos.trade_id == trade.id
        assert s.query(OrderRecord).filter(OrderRecord.symbol == SYMBOL).count() == 1
    _cleanup(pid)


def test_persist_close_partial_keeps_position_open():
    pid = _position(qty=2.0, entry=100.0)

    broker_mod._persist_close(_payload(close_qty=1.0, pnl=0.9, fee=0.05), pid)  # noqa: SLF001

    with session_scope() as s:
        pos = s.get(PositionRecord, pid)
        assert pos.status == "open"
        assert pos.qty == pytest.approx(1.0)
        assert pos.realized_pnl == pytest.approx(0.9)
        trade = s.query(TradeRecord).filter(TradeRecord.symbol == SYMBOL).one()
        assert trade.qty == pytest.approx(1.0)
        # 部分平仓也要按比例摊保证金算 ROI，否则 ROI 会偏低一半
        assert trade.roi_pct != 0
    _cleanup(pid)


def test_persist_close_on_closed_position_is_noop():
    pid = _position()
    with session_scope() as s:
        s.get(PositionRecord, pid).status = "closed"

    broker_mod._persist_close(_payload(), pid)  # noqa: SLF001

    with session_scope() as s:
        assert s.query(TradeRecord).filter(TradeRecord.symbol == SYMBOL).count() == 0
    _cleanup(pid)


# ------------------------------------------------------------ 下单
class _FakeClient:
    def __init__(self, *, avg: float = 101.0, fail: BaseException | None = None) -> None:
        self.avg = avg
        self.fail = fail
        self.orders: list[dict] = []
        self.cancels: list[str] = []

    async def symbol_filters(self):
        return {
            SYMBOL: SymbolFilters(
                symbol=SYMBOL, price_tick=0.01, qty_step=0.001, min_qty=0.001, min_notional=5.0
            )
        }

    async def place_order(self, **kw):
        if self.fail is not None:
            raise self.fail
        self.orders.append(kw)
        return {"avgPrice": str(self.avg), "status": "FILLED", "clientOrderId": "nx-1", "orderId": 7}

    async def cancel_all(self, symbol):
        self.cancels.append(symbol)
        return {}

    async def cancel_all_algo(self, symbol):
        self.cancels.append(f"algo:{symbol}")
        return {}


@pytest.fixture()
def patched(monkeypatch):
    client = _FakeClient()
    monkeypatch.setattr(broker_mod.binance_client, "client", client)
    submitted: list[tuple] = []

    def _submit(fn, *args, **kwargs):
        submitted.append((fn, args, kwargs))
        fn(*args, **kwargs)  # 同步执行，测试里不要真起写线程

    monkeypatch.setattr(broker_mod.db_writer, "submit", _submit)
    yield client, submitted
    monkeypatch.undo()


def _broker() -> broker_mod.BinanceBroker:
    brk = broker_mod.BinanceBroker()
    brk._dual = True  # 跳过持仓模式探测  # noqa: SLF001
    return brk


def test_close_rejects_missing_position(patched):
    brk = _broker()
    fake_pos = SimpleNamespace(id=999999, symbol=SYMBOL, side="long", qty=1.0,
                               entry_price=100.0, mark_price=101.0)
    out = asyncio.run(brk.close_position(position=fake_pos, reason="manual"))
    assert out.ok is False and "不存在" in out.message


def test_close_is_mutually_exclusive(patched):
    """引擎轮询与手动平仓同时触发时，只允许一个下单."""
    pid = _position()
    broker_mod._CLOSING.add(pid)  # noqa: SLF001
    try:
        out = asyncio.run(_broker().close_position(position=_detached(pid), reason="manual"))
        assert out.ok is False and "正在平仓" in out.message
        assert patched[0].orders == []
    finally:
        _cleanup(pid)


def test_close_rejects_qty_below_min(patched):
    pid = _position(qty=0.0005)
    try:
        out = asyncio.run(_broker().close_position(position=_detached(pid), reason="manual"))
        assert out.ok is False and "最小下单量" in out.message
        assert pid not in broker_mod._CLOSING, "被拒后必须释放互斥锁，否则仓位永远平不掉"
    finally:
        _cleanup(pid)


def test_full_close_places_market_order_and_cancels_protection(patched):
    client, submitted = patched
    pid = _position(qty=2.0, entry=100.0, mark=100.5)
    try:
        out = asyncio.run(_broker().close_position(position=_detached(pid), reason="take_profit"))

        assert out.ok is True
        assert out.qty == pytest.approx(2.0)
        order = client.orders[0]
        assert order["side"] == "SELL"
        assert order["type"] == "MARKET"
        assert order["quantity"] == "2.000"
        # 全平才撤保护单
        assert SYMBOL in client.cancels and f"algo:{SYMBOL}" in client.cancels
        assert submitted, "平仓必须落库（否则账本与交易所长期不一致）"
    finally:
        _cleanup(pid)


def test_partial_close_keeps_protection_orders(patched):
    client, _ = patched
    pid = _position(qty=2.0)
    try:
        out = asyncio.run(
            _broker().close_position(position=_detached(pid), reason="partial_tp", qty=1.0)
        )

        assert out.ok is True and out.qty == pytest.approx(1.0)
        assert client.cancels == [], "部分平仓撤掉止损/止盈，剩余仓位就裸奔了"
    finally:
        _cleanup(pid)


def test_close_without_credentials_reports_clearly(monkeypatch):
    monkeypatch.setattr(broker_mod.binance_client, "client", _FakeClient(fail=NoCredentials("未配置 API Key")))
    pid = _position()
    try:
        out = asyncio.run(_broker().close_position(position=_detached(pid), reason="manual"))
        assert out.ok is False and "API Key" in out.message
        assert pid not in broker_mod._CLOSING
    finally:
        _cleanup(pid)


def test_close_binance_error_is_reported_and_unlocks(monkeypatch):
    monkeypatch.setattr(
        broker_mod.binance_client, "client", _FakeClient(fail=BinanceError(-2022, "ReduceOnly Order is rejected"))
    )
    pid = _position()
    try:
        out = asyncio.run(_broker().close_position(position=_detached(pid), reason="manual"))
        assert out.ok is False and "-2022" in out.message
        assert pid not in broker_mod._CLOSING
    finally:
        _cleanup(pid)


def test_close_unexpected_exception_unlocks_and_raises(monkeypatch):
    monkeypatch.setattr(broker_mod.binance_client, "client", _FakeClient(fail=RuntimeError("boom")))
    pid = _position()
    try:
        with pytest.raises(RuntimeError):
            asyncio.run(_broker().close_position(position=_detached(pid), reason="manual"))
        assert pid not in broker_mod._CLOSING, "未知异常也必须解锁，否则该仓位再也平不掉"
    finally:
        _cleanup(pid)
