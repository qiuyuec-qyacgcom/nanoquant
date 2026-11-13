"""回测真实性：强平优先于止损、跳空按开盘价成交.

为什么单独测：8–30x 杠杆下，"把爆仓记成普通止损"会让最大回撤和期望都失真；
"止损永远按止损价成交"是最经典的乐观假设，插针行情里能把亏损算小一大截。
"""
from __future__ import annotations

from types import SimpleNamespace

from nanoquant.backtest import engine as bt
from nanoquant.exchange.client import Candle
from nanoquant.services.settings import StrategySettings

BAR_MS = 300_000  # 5m


def _flat(n: int, price: float = 100.0) -> list[Candle]:
    return [
        Candle(open_time=i * BAR_MS, open=price, high=price, low=price, close=price, volume=1000.0)
        for i in range(n)
    ]


def _bar(idx: int, open_: float, high: float, low: float, close: float) -> Candle:
    return Candle(open_time=idx * BAR_MS, open=open_, high=high, low=low, close=close, volume=1000.0)


def _sig(direction: str = "long", sl_pct: float = 1.0, tp_pct: float = 0.0) -> SimpleNamespace:
    return SimpleNamespace(direction=direction, score=0.9, sl_pct=sl_pct, tp_pct=tp_pct)


def _run(monkeypatch, candles, **kwargs):
    """固定产出 1% 止损的多头信号；滑点/手续费/资金费置 0，便于精确断言成交价。"""
    monkeypatch.setattr(bt, "score_signal", lambda *a, **k: _sig())
    cfg = StrategySettings(trailing_pct=0.0, partial_tp_at_r=0.0, roi_table=[], max_hold_minutes=2880)
    defaults = dict(
        symbol="TESTUSDT", fee_bps=0.0, slippage_bps=0.0, spread_bps=0.0, funding_bps_8h=0.0,
        risk_per_trade_pct=1.0, margin_pct=100.0,
    )
    defaults.update(kwargs)
    return bt.run_backtest(candles, cfg, **defaults)


def _setup(tail: list[Candle]) -> list[Candle]:
    """60 根横盘 + 一根入场 K 线（开盘 100，不动）+ 尾部行情。

    i=60 出信号、按第 61 根开盘 100 入场；第 61 根不触发任何出场，
    这样尾部第一根才是真正被测的那根 K 线。
    """
    candles = _flat(61)
    candles.append(_bar(61, 100.0, 100.0, 100.0, 100.0))
    candles.extend(tail)
    return candles


def test_liquidation_fires_before_stop_loss(monkeypatch):
    """50x 下 1.5% 的反向插针先爆仓，不该记成 -1R 止损。"""
    candles = _setup([_bar(62, 100.0, 100.0, 96.0, 96.0), _bar(63, 96.0, 96.0, 96.0, 96.0)])

    result = _run(monkeypatch, candles, leverage=50.0, maintenance_margin_rate=0.005)

    assert result.trades, "应当有成交"
    trade = result.trades[0]
    assert trade.reason == "liquidation"
    # 强平价 = 100 × (1 − (1/50 − 0.005)) = 98.5；本根开盘 100 未跳空，按强平价成交
    assert trade.exit == 98.5
    assert result.summary["liquidations"] == 1
    assert result.summary["leverage"] == 50.0


def test_stop_loss_fill_uses_gap_open(monkeypatch):
    """跳空低开穿过止损价时，只能按开盘价成交（旧实现按止损价，亏少了）。"""
    candles = _setup([_bar(62, 95.0, 95.5, 94.0, 95.0), _bar(63, 95.0, 95.0, 95.0, 95.0)])

    result = _run(monkeypatch, candles, leverage=1.0)

    trade = result.trades[0]
    assert trade.reason == "stop_loss"
    assert trade.exit == 95.0, f"应按跳空开盘价 95 成交，实际 {trade.exit}"
    assert trade.exit < trade.entry * 0.99  # 止损价 99，实际成交更差


def test_low_leverage_keeps_normal_stop(monkeypatch):
    """低杠杆时强平价很远，普通止损照常生效，且未跳空就按止损价成交。"""
    candles = _setup([_bar(62, 99.5, 99.5, 96.5, 97.0), _bar(63, 97.0, 97.0, 97.0, 97.0)])

    result = _run(monkeypatch, candles, leverage=3.0)

    trade = result.trades[0]
    assert trade.reason == "stop_loss"
    assert trade.exit == 99.0  # 开盘 99.5 未穿过止损 99，按止损价成交
    assert result.summary["liquidations"] == 0
