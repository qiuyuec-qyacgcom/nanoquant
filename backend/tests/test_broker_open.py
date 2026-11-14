"""开仓链（broker.open_position / add_to_position / amend_stop）.

与平仓同样重要：下单参数写错（方向、positionSide、reduceOnly、精度）会直接被拒，
或者更糟——在双向持仓账户里开出一个反向仓位。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.exchange import broker as broker_mod
from nanoquant.exchange.client import BinanceError, NoCredentials, UncertainSubmission
from nanoquant.exchange.precision import SymbolFilters
from nanoquant.models import PositionRecord
from nanoquant.services.settings import get_engine, update_group

SYMBOL = "OPENTESTUSDT"


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _filters(price: float = 100.0) -> SymbolFilters:
    return SymbolFilters(
        symbol=SYMBOL, price_tick=0.01, qty_step=0.001, min_qty=0.001,
        min_notional=5.0, last_price=price, quote_volume_24h=5e7,
    )


class _FakeClient:
    def __init__(self, *, avg: float = 100.0, ask: float = 100.0, bid: float = 99.99,
                 fail: BaseException | None = None, resolve: dict | None = None) -> None:
        self.avg = avg
        self.ask = ask
        self.bid = bid
        self.fail = fail
        self.resolve = resolve
        self.orders: list[dict] = []
        self.algos: list[dict] = []
        self.margin_calls: list[tuple] = []
        self.leverage_calls: list[tuple] = []
        self.cancels: list[str] = []

    async def symbol_filters(self):
        return {SYMBOL: _filters()}

    async def set_margin_type(self, symbol, kind):
        self.margin_calls.append((symbol, kind))
        return {}

    async def set_leverage(self, symbol, lev):
        self.leverage_calls.append((symbol, lev))
        return {}

    async def book_tickers(self, symbols):
        return {s: (self.bid, self.ask) for s in symbols}

    async def place_order(self, **kw):
        if self.fail is not None:
            raise self.fail
        self.orders.append(kw)
        return {
            "avgPrice": str(self.avg), "executedQty": str(kw["quantity"]),
            "status": "FILLED", "orderId": 123, "clientOrderId": kw.get("newClientOrderId", ""),
        }

    async def place_algo_order(self, **kw):
        self.algos.append(kw)
        return {"algoId": 1}

    async def get_order(self, symbol, client_id=None, order_id=None):
        if self.resolve is None:
            raise BinanceError(-2013, "Order does not exist")
        return self.resolve

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
        fn(*args, **kwargs)

    monkeypatch.setattr(broker_mod.db_writer, "submit", _submit)
    yield client, submitted
    with session_scope() as s:
        s.query(PositionRecord).filter(PositionRecord.symbol == SYMBOL).delete(synchronize_session=False)


def _broker() -> broker_mod.BinanceBroker:
    brk = broker_mod.BinanceBroker()
    brk._dual = True  # noqa: SLF001
    return brk


def _open(client=None, **over):
    kw = {
        "symbol": SYMBOL, "side": "long", "qty": 0.5, "leverage": 10,
        "sl_price": 98.0, "tp_price": 104.0, "filters": _filters(),
        "ai_confidence": 0.7, "ai_reason": "test", "signal_score": 0.8, "features": {"k": 1},
    }
    kw.update(over)
    return asyncio.run(_broker().open_position(**kw))


# ------------------------------------------------------------ 参数与落库
def test_hedge_order_params_respects_account_mode():
    assert broker_mod.hedge_order_params(True, "long") == {"positionSide": "LONG"}
    assert broker_mod.hedge_order_params(True, "short") == {"positionSide": "SHORT"}
    assert broker_mod.hedge_order_params(False, "long", closing=True) == {"reduceOnly": "true"}
    assert broker_mod.hedge_order_params(False, "long") == {}


def test_open_places_market_order_and_persists(patched):
    client, submitted = patched

    out = _open()

    assert out.ok is True
    order = client.orders[0]
    assert order["side"] == "BUY"
    assert order["type"] == "MARKET"
    assert order["quantity"] == "0.500"
    assert order["positionSide"] == "LONG"
    # 逐仓 + 杠杆必须先设好，否则下单按账户默认杠杆执行
    assert client.margin_calls == [(SYMBOL, "ISOLATED")]
    assert client.leverage_calls == [(SYMBOL, 10)]
    # 止损止盈各挂一次条件单
    assert len(client.algos) == 2
    kinds = {a["type"] for a in client.algos}
    assert kinds == {"STOP_MARKET", "TAKE_PROFIT_MARKET"}
    assert submitted, "开仓必须落库"
    with session_scope() as s:
        row = s.query(PositionRecord).filter(PositionRecord.symbol == SYMBOL).one()
        assert row.qty == pytest.approx(0.5)
        assert row.side == "long"
        assert row.leverage == pytest.approx(10)


def test_open_skips_protection_when_prices_missing(patched):
    client, _ = patched

    out = _open(sl_price=0.0, tp_price=0.0)

    assert out.ok is True
    assert client.algos == []


def test_open_rejects_when_book_moved_too_far(patched):
    """盘口偏离决策价超过 max_slippage_pct 就放弃——短线这一跳就是利润."""
    client, submitted = patched
    client.ask = 101.0  # 距 100 有 100bps，默认上限 25bps

    out = _open()

    assert out.ok is False and "偏离" in out.message
    assert client.orders == [] and submitted == []


def test_open_post_only_uses_limit_gtx(patched):
    client, _ = patched
    prev = get_engine().order_type
    update_group("engine", {"order_type": "limit_post_only"})
    try:
        out = _open()
    finally:
        update_group("engine", {"order_type": prev})

    assert out.ok is True
    order = client.orders[0]
    assert order["type"] == "LIMIT" and order["timeInForce"] == "GTX"
    assert order["price"] == "100.00"


# ------------------------------------------------------------ 错误映射
def test_open_maps_margin_error_to_actionable_hint(monkeypatch):
    monkeypatch.setattr(
        broker_mod.binance_client, "client", _FakeClient(fail=BinanceError(-2019, "Margin is insufficient"))
    )
    out = _open()
    assert out.ok is False
    assert "保证金不足" in out.message and "初始保证金" in out.message


def test_open_maps_precision_error_to_hint(monkeypatch):
    monkeypatch.setattr(
        broker_mod.binance_client, "client", _FakeClient(fail=BinanceError(-1111, "Precision is over the maximum"))
    )
    out = _open()
    assert out.ok is False and "精度" in out.message


def test_open_passes_through_other_errors(monkeypatch):
    monkeypatch.setattr(
        broker_mod.binance_client, "client", _FakeClient(fail=BinanceError(-4131, "PERCENT_PRICE"))
    )
    out = _open()
    assert out.ok is False and "-4131" in out.message


def test_open_without_credentials(monkeypatch):
    monkeypatch.setattr(broker_mod.binance_client, "client", _FakeClient(fail=NoCredentials("未配置 API Key")))
    out = _open()
    assert out.ok is False and "API Key" in out.message


def test_uncertain_submission_is_resolved_by_query(monkeypatch):
    """提交结果未知时**不能重发**，必须用 clientOrderId 查回来."""
    client = _FakeClient(fail=UncertainSubmission(-1001, "timeout"))
    client.resolve = {"avgPrice": "100.5", "executedQty": "0.5", "status": "FILLED", "orderId": 556}
    monkeypatch.setattr(broker_mod.binance_client, "client", client)
    submitted: list = []
    monkeypatch.setattr(broker_mod.db_writer, "submit", lambda fn, *a, **k: submitted.append(fn(*a, **k)))

    out = _open()

    assert out.ok is True
    assert out.price == pytest.approx(100.5)
    assert out.order_id == "556"
    assert len(client.orders) == 0, "结果未知时绝不能重发一次"


def test_uncertain_submission_without_query_result_fails_closed(monkeypatch):
    client = _FakeClient(fail=UncertainSubmission(-1001, "timeout"))
    client.resolve = None  # 查询也查不到

    async def _no_sleep(_sec):
        return None

    monkeypatch.setattr(asyncio, "sleep", _no_sleep)  # 别为 4 次重试真等 4.8 秒
    monkeypatch.setattr(broker_mod.binance_client, "client", client)
    monkeypatch.setattr(broker_mod.db_writer, "submit", lambda *a, **k: None)

    out = _open()

    assert out.ok is False and "结果未知" in out.message


# ------------------------------------------------------------ 改止损
def test_amend_stop_reaplies_take_profit(patched):
    client, _ = patched

    out = asyncio.run(_broker().amend_stop(SYMBOL, "long", 99.5, 0.5, tp_price=104.0))

    assert out.ok is True
    assert SYMBOL in client.cancels
    kinds = {a["type"] for a in client.algos}
    assert kinds == {"STOP_MARKET", "TAKE_PROFIT_MARKET"}, "只挂止损会把止盈单撤掉不再补"
    stop = next(a for a in client.algos if a["type"] == "STOP_MARKET")
    assert stop["triggerPrice"] == "99.50"
