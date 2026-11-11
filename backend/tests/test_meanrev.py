# -*- coding: utf-8 -*-
"""均值回归模式与参数网格的护栏。"""
from __future__ import annotations

from types import SimpleNamespace

import pytest

from nanoquant.backtest import grid as grid_mod
from nanoquant.engine.strategy import build_features, score_signal
from nanoquant.exchange.client import Candle
from nanoquant.services.settings import StrategySettings


def _candles(n: int = 60) -> list[Candle]:
    out, price = [], 100.0
    for i in range(n):
        price += 0.1
        out.append(Candle(open_time=i * 60, open=price - 0.1, high=price + 0.2,
                          low=price - 0.2, close=price, volume=100.0))
    return out


def _fed(cfg: StrategySettings, candles: list[Candle], **kw):
    return build_features(
        candles, cfg, funding=kw.get("funding", 0.0001),
        oi_change_pct=kw.get("oi", 0.5), quote_volume=kw.get("qv", 5e7),
    )


@pytest.fixture
def mr():
    return StrategySettings(mode="meanrev", timeframe="5m", mr_block_in_trend=False)


def test_meanrev_fades_an_extreme_move_upward(mr):
    """价格大幅偏离中轨之上 → 做空（赌回归）。"""
    candles = _candles(60)
    # 最后一根暴力拉升，把 z 推到高位
    candles[-1] = Candle(open_time=candles[-1].open_time, open=105, high=112,
                         low=104, close=111, volume=100.0)
    f = _fed(mr, candles)
    sig = score_signal("X", f, mr)
    assert f["band_z"] > 2
    assert sig.direction == "short", sig.blocked
    assert sig.score < 0
    # 止盈回到中轨附近，止损在极值之外
    assert sig.tp_pct < abs(f["band_z"]) * f["band_sd_pct"] + 1e-6
    assert sig.sl_pct > sig.tp_pct * 0.5


def test_meanrev_stays_flat_when_deviation_is_small(mr):
    candles = _candles(60)
    f = _fed(mr, candles)
    sig = score_signal("X", f, mr)
    assert abs(f["band_z"]) < mr.mr_z_entry
    assert sig.direction == "flat"
    assert "偏离不足" in sig.blocked


def test_meanrev_refuses_counter_trend_in_strong_uptrend():
    """强趋势里做逆势回归 = 接飞刀，必须拦下。"""
    cfg = StrategySettings(mode="meanrev", mr_block_in_trend=True, mr_trend_gap_pct=0.05)
    candles = []
    price = 100.0
    for i in range(60):
        price *= 1.004  # 每根 +0.4%，EMA 间距必然拉开
        candles.append(Candle(open_time=i * 60, open=price * 0.998, high=price * 1.002,
                              low=price * 0.996, close=price, volume=100.0))
    f = _fed(cfg, candles)
    sig = score_signal("X", f, cfg)
    assert sig.direction == "flat"
    assert "趋势中" in sig.blocked


def test_meanrev_discounts_high_volume_breakout():
    """放量突破不是超买：同样偏离下，放量的置信度应该更低。"""
    cfg = StrategySettings(mode="meanrev", mr_block_in_trend=False)
    quiet = _candles(60)
    loud = _candles(60)
    for arr, vol in ((quiet, 100.0), (loud, 400.0)):
        arr[-1] = Candle(open_time=arr[-1].open_time, open=105, high=112,
                         low=104, close=111, volume=vol)
    s_quiet = score_signal("X", _fed(cfg, quiet), cfg)
    s_loud = score_signal("X", _fed(cfg, loud), cfg)
    assert abs(s_loud.score) < abs(s_quiet.score)


# ------------------------------------------------------------------ 网格护栏

def test_grid_ignores_thin_samples(monkeypatch):
    """2 笔单赚 50% 不能当选最优 —— 那是过拟合最廉价的入口。"""

    def fake_run(_candles, cfg, **_kw):
        if cfg.entry_threshold == 0.9:
            return SimpleNamespace(summary={
                "trades": 2, "win_rate": 100.0, "profit_factor": 9.0, "expectancy": 5.0,
                "total_return_pct": 50.0, "max_drawdown_pct": 1.0, "avg_bars": 1.0,
            })
        return SimpleNamespace(summary={
            "trades": 40, "win_rate": 45.0, "profit_factor": 1.3, "expectancy": 0.2,
            "total_return_pct": 8.0, "max_drawdown_pct": 6.0, "avg_bars": 5.0,
        })

    monkeypatch.setattr(grid_mod, "run_backtest", fake_run)
    out = grid_mod.run_grid(
        [1] * 100, StrategySettings(), grid={"entry_threshold": [0.9, 0.3]}
    )
    assert out["min_trades"] == grid_mod.MIN_TRADES
    # 明细里仍保留高收益的薄样本（供人看），但 best 必须是样本充足的那个
    assert out["items"][0]["params"]["entry_threshold"] == 0.9
    assert out["best"]["params"]["entry_threshold"] == 0.3


def test_grid_accepts_string_params():
    """mode 这类字符串参数要能进网格，否则"换假设"没法验证。"""
    assert grid_mod._round_param("meanrev") == "meanrev"
    assert grid_mod._round_param(1.23456) == 1.2346
    assert grid_mod._round_param(True) is True


def test_grid_keys_cover_mode_and_meanrev_params():
    for key in ("mode", "mr_z_entry", "mr_min_risk_reward", "min_risk_reward", "roi_min_r"):
        assert key in grid_mod.GRID_KEYS


# ------------------------------------------------------------------ 默认值

def test_auto_entry_is_off_by_default():
    """样本外没有任何正期望配置时，默认不该自动开仓。"""
    assert StrategySettings().auto_entry is False


def test_default_mode_still_trend_but_longer_timeframe():
    cfg = StrategySettings()
    assert cfg.mode == "trend"
    assert cfg.timeframe == "30m"
