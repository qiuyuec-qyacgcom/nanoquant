"""挂单入场：保护单失败必须可见；成交后由系统自动补挂.

两个真实缺陷（用户实测）：
1. 挂单时预挂 closePosition 保护单常被交易所拒绝（尚无持仓），
   旧实现只 `log.warning`，用户完全不知道仓位会裸奔。
2. 挂单成交后 `_reconcile_live` 把它当「外部仓位」，
   默认 `auto_protect_external=False` → 不补挂，仓位一直裸奔。
"""
import asyncio
import time

import pytest

from nanoquant.exchange import broker as broker_mod
from nanoquant.exchange.broker import BinanceBroker


class _FailAlgo:
    """所有 Algo 单都 reject。"""

    def __init__(self):
        self.calls = 0

    async def place_algo_order(self, **kw):
        self.calls += 1
        raise broker_mod.BinanceError(-4120, "Order type not supported for this endpoint")

    async def place_order(self, **kw):
        self.calls += 1
        raise broker_mod.BinanceError(-1102, "Mandatory parameter missing")


def test_place_protection_returns_failures(monkeypatch):
    """保护单失败必须作为返回值暴露，而不是只写日志。"""
    fake = _FailAlgo()
    monkeypatch.setattr(broker_mod.binance_client, "client", fake)

    b = BinanceBroker()
    errs = asyncio.run(
        b._place_protection("BTCUSDT", "long", 73000.0, 75000.0, None, dual=False)
    )
    assert isinstance(errs, list)
    assert len(errs) == 2, errs
    assert all(err for err in errs)
    assert fake.calls >= 2


def test_place_protection_success_returns_empty(monkeypatch):
    class Ok:
        async def place_algo_order(self, **kw):
            return {"algoId": 1}

        async def place_order(self, **kw):
            return {"orderId": 1}

    monkeypatch.setattr(broker_mod.binance_client, "client", Ok())
    b = BinanceBroker()
    errs = asyncio.run(
        b._place_protection("BTCUSDT", "long", 73000.0, 75000.0, None, dual=False)
    )
    assert errs == [], "全部成功时必须返回空列表"


def test_recent_entry_order_detects_our_own_order():
    """有本系统 entry 委托 → 判定为「我们挂单成交」。"""
    from nanoquant.db import session_scope
    from nanoquant.engine.worker import TradingEngine
    from nanoquant.models import OrderRecord

    symbol = "PENDTESTUSDT"
    with session_scope() as s:
        s.query(OrderRecord).filter(OrderRecord.symbol == symbol).delete()
        s.add(
            OrderRecord(
                ts=int(time.time()),
                symbol=symbol,
                side="BUY",
                order_type="LIMIT",
                qty=0.0,
                price=0.0,
                status="NEW",
                purpose="entry",
                client_order_id="nq_pending_1",
                mode="demo",
            )
        )
    try:
        assert TradingEngine._recent_entry_order(symbol, "demo") is True
        assert TradingEngine._recent_entry_order(symbol, "live") is False, "mode 要隔离"
        assert TradingEngine._recent_entry_order("NOSUCHUSDT", "demo") is False
    finally:
        with session_scope() as s:
            s.query(OrderRecord).filter(OrderRecord.symbol == symbol).delete()


def test_resting_entry_returns_pending_not_failure(monkeypatch):
    """挂单未成交：ok=False 但 pending=True，且不回退成 error 级失败文案。"""
    from nanoquant.exchange.broker import ExecResult

    res = ExecResult(False, message="挂单已提交，等待成交（NEW）", pending=True)
    assert res.ok is False and res.pending is True
    assert "失败" not in res.message
