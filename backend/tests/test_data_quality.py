"""数据完整性闸门：缺口与停更必须在打分前拦掉.

为什么单独测：断档时 EMA/ATR/布林依然会算出"看起来完全正常"的数值，
信号照发——这类问题不报错、只亏钱，比崩溃更难发现。
"""
from __future__ import annotations

from nanoquant.engine.strategy import build_features, score_signal
from nanoquant.exchange.client import Candle
from nanoquant.services.settings import StrategySettings

STEP = 900  # 15m


def _candles(n: int = 80, *, skip_at: int | None = None, price: float = 100.0) -> list[Candle]:
    """构造连续 K 线；skip_at 指定"跳过第 i 根"制造一个缺口。"""
    out: list[Candle] = []
    t = 0
    for i in range(n):
        if skip_at is not None and i == skip_at:
            t += STEP  # 时间前进但不产出这根 → 缺口
            continue
        p = price * (1 + 0.0008 * i)
        out.append(Candle(open_time=t, open=p, high=p * 1.002, low=p * 0.998, close=p, volume=100.0))
        t += STEP
    return out


def _cfg(**kwargs) -> StrategySettings:
    params = {"mode": "meanrev", "max_data_gaps": 0, "data_stale_mult": 2.0}
    params.update(kwargs)
    return StrategySettings(**params)


def test_contiguous_fresh_data_passes():
    candles = _candles()
    last_open = int(candles[-1].open_time)
    now = last_open + STEP  # 刚收盘
    f = build_features(candles, _cfg(), now=now)

    assert f["data_gaps"] == 0
    assert f["data_ok"] is True
    assert f["data_age_sec"] is not None and f["data_age_sec"] < STEP


def test_gap_is_detected_and_blocks_signal():
    candles = _candles(skip_at=60)
    f = build_features(candles, _cfg(), now=int(candles[-1].open_time) + STEP)

    assert f["data_gaps"] >= 1
    assert f["data_ok"] is False
    assert "缺口" in f["data_note"]

    sig = score_signal("TESTUSDT", f, _cfg())
    assert sig.direction == "flat"
    assert "缺口" in sig.blocked


def test_gap_can_be_tolerated_by_config():
    """低流动性标的偶发断档：允许 1 处时不拦，但缺口数仍然可见。"""
    candles = _candles(skip_at=60)
    cfg = _cfg(max_data_gaps=1)
    f = build_features(candles, cfg, now=int(candles[-1].open_time) + STEP)
    assert f["data_gaps"] == 1
    assert f["data_ok"] is True


def test_stale_feed_blocks_signal():
    candles = _candles()
    last_open = int(candles[-1].open_time)
    now = last_open + STEP + STEP * 5  # 收完之后又过了 5 根周期
    f = build_features(candles, _cfg(), now=now)

    assert f["data_ok"] is False
    assert "停更" in f["data_note"]
    assert score_signal("TESTUSDT", f, _cfg()).blocked.startswith("行情数据不可信")


def test_historical_data_is_not_treated_as_stale():
    """回测不传 now：历史 K 线永远是"旧的"，不能因此判停更。"""
    candles = _candles()
    f = build_features(candles, _cfg())
    assert f["data_age_sec"] is None
    assert f["data_ok"] is True
