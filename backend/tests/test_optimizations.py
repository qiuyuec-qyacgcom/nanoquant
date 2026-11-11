# -*- coding: utf-8 -*-
"""全链路优化新增能力的回归测试。"""
from __future__ import annotations

import time

import pytest

from nanoquant.engine.risk import categorize, category_exposure_pct
from nanoquant.engine.strategy import build_features, score_signal
from nanoquant.exchange.client import Candle
from nanoquant.services.settings import StrategySettings


def _candles(n: int = 60, step: float = 0.2, vol: float = 100.0) -> list[Candle]:
    out, price = [], 100.0
    for i in range(n):
        price += step
        out.append(Candle(open_time=i * 60, open=price - step, high=price + 0.3,
                          low=price - 0.3, close=price, volume=vol))
    return out


def _fed(cfg, candles, **kw):
    return build_features(candles, cfg, funding=kw.get("funding", 0.0),
                          oi_change_pct=kw.get("oi", 0.5), quote_volume=5e7)


# ------------------------------------------------------------------ ADX 过滤

def test_adx_filter_blocks_weak_trend():
    """趋势强度不足时，方向分再高也不放行。直接注入低 ADX 测闸门。"""
    cfg = StrategySettings(mode="trend", adx_filter=True, adx_min=25.0)
    f = _fed(cfg, _candles(60, step=0.4))
    f["adx"] = 10.0  # 注入低 ADX，与是否真能构造出低 ADX 解耦
    f["ok"]["adx"] = True
    sig = score_signal("X", f, cfg)
    assert sig.direction == "flat"
    assert "趋势强度不足" in sig.blocked


def test_adx_filter_off_allows():
    cfg = StrategySettings(mode="trend", adx_filter=False)
    f = _fed(cfg, _candles(60, step=0.4))
    sig = score_signal("X", f, cfg)
    assert "趋势强度不足" not in sig.blocked


# ------------------------------------------------------------------ HTF 过滤

def test_htf_filter_blocks_counter_trend():
    cfg = StrategySettings(mode="trend", adx_filter=False)
    candles = _candles(60, step=0.4)  # 上涨 → 多头信号
    f = _fed(cfg, candles)
    sig = score_signal("X", f, cfg)
    assert sig.direction == "long"

    # HTF 向下时，多头信号应被拦下
    f2 = _fed(cfg, candles)
    f2["htf_trend"] = "down"
    sig2 = score_signal("X", f2, cfg)
    assert sig2.direction == "flat"
    assert "逆高周期趋势" in sig2.blocked


# ------------------------------------------------------------------ meanrev 衰竭确认

def test_meanrev_exhaustion_confirm_blocks_high_volume():
    cfg = StrategySettings(mode="meanrev", mr_block_in_trend=False,
                           mr_exhaustion_confirm=True)
    f = _fed(cfg, _candles(60))
    # 直接注入一个"放量推极值"的状态：z 够高、量比放大
    f["band_z"] = 2.5
    f["band_sd_pct"] = 1.0
    f["vol_ratio"] = 1.8
    f["ema_gap_pct"] = 0.1
    f["close"] = 100.0
    f["ok"]["meanrev"] = True
    sig = score_signal("X", f, cfg)
    assert sig.direction == "flat"
    assert "动能未衰竭" in sig.blocked


# ------------------------------------------------------------------ 相关性敞口

def test_categorize_buckets():
    assert categorize("BTCUSDT") == "btc"
    assert categorize("1000PEPEUSDT") == "alt"
    assert categorize("ETHUSDT") == "eth"
    assert categorize("SOLUSDT") == "major"
    assert categorize("ARIAUSDT") == "alt"


def test_category_exposure_pct(monkeypatch):
    from nanoquant.engine import risk as risk_mod

    class _P:
        def __init__(self, symbol, qty, mark):
            self.symbol, self.qty, self.mark_price, self.entry_price = symbol, qty, mark, mark
            self.side, self.mode, self.status = "long", "demo", "open"

    monkeypatch.setattr(risk_mod, "open_positions",
                        lambda: [_P("BTCUSDT", 1, 100), _P("SOLUSDT", 1, 50), _P("ARIAUSDT", 1, 10)])
    class _A:
        equity = 100.0
    exp = category_exposure_pct({}, _A())
    assert exp["btc"] == pytest.approx(100.0)   # 1 × 100 / 100
    assert exp["major"] == pytest.approx(50.0)
    assert exp["alt"] == pytest.approx(10.0)


# ------------------------------------------------------------------ 数据降级

def test_data_degraded_surfaces_in_reasons():
    cfg = StrategySettings(mode="trend", adx_filter=False)
    f = _fed(cfg, _candles(60, step=0.4))
    f["data_degraded"] = True
    sig = score_signal("X", f, cfg)
    assert any("REST" in r for r in sig.reasons)


# ------------------------------------------------------------------ 策略指纹覆盖新参数

def test_fingerprint_sensitive_to_adx_and_htf():
    from nanoquant.engine.strategy import strategy_fingerprint
    base = strategy_fingerprint(StrategySettings())
    assert strategy_fingerprint(StrategySettings(adx_filter=False)) != base
    assert strategy_fingerprint(StrategySettings(htf_timeframe="1h")) != base
