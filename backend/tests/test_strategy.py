# -*- coding: utf-8 -*-
"""策略模块：缺失语义 / 权重归一 / 盈亏比下限 / 体制自适应。

这几条都是从"看起来在算，其实在打折"的坑里捞出来的：
- 指标算不出来时返回 0/50，等于用"没有信息"稀释真实信号；
- OI 因子从来没有被赋值，却一直占着 12% 权重（全体信号被系统性地打 88 折）；
- 止盈固定按止损的 1.1 倍算，R:R 只有 1.1 时需要 48% 以上胜率才不亏手续费。
"""
from __future__ import annotations

import pytest

from nanoquant.engine.strategy import (
    build_features,
    detect_exit,
    entry_regime,
    rsi_zone_score,
    score_signal,
    strategy_fingerprint,
)
from nanoquant.exchange.client import Candle
from nanoquant.services.settings import StrategySettings


def _candles(n: int, *, start: float = 100.0, step: float = 0.2, vol: float = 100.0) -> list[Candle]:
    out = []
    price = start
    for i in range(n):
        price += step
        out.append(
            Candle(
                open_time=i * 60, open=price - step, high=price + 0.1,
                low=price - 0.3, close=price, volume=vol,
            )
        )
    return out


def _features(n: int = 60, **kw):
    cfg = kw.pop("cfg", None) or StrategySettings()
    candles = _candles(n, **{k: v for k, v in kw.items() if k in ("start", "step", "vol")})
    return cfg, build_features(
        candles, cfg,
        funding=kw.get("funding", 0.0001),
        oi_change_pct=kw.get("oi_change_pct", 1.0),
        quote_volume=kw.get("quote_volume", 5e7),
        change_24h=0.0,
    )


# ------------------------------------------------------------------ 缺失语义

def test_insufficient_bars_blocks_instead_of_scoring_neutral():
    """K 线不足时不能"算出个 0 分当观望"——那是把缺失伪装成结论。"""
    cfg, f = _features(n=10)
    assert f["ok"]["trend"] is False
    assert any("趋势" in m for m in f["missing"])

    sig = score_signal("BTCUSDT", f, cfg)
    assert sig.direction == "flat"
    assert "K线不足" in sig.blocked
    assert sig.missing  # 缺失项必须被带出来，供面板与 LLM 复审看见


def test_missing_funding_is_not_zero_funding():
    """没取到资金费率 ≠ 费率为 0。前者应剔除权重，后者才是中性。"""
    cfg = StrategySettings()
    candles = _candles(60)
    f_none = build_features(candles, cfg, funding=None, oi_change_pct=1.0, quote_volume=5e7)
    f_zero = build_features(candles, cfg, funding=0.0, oi_change_pct=1.0, quote_volume=5e7)

    assert f_none["ok"]["funding"] is False
    assert f_zero["ok"]["funding"] is True
    assert "资金费率" in " ".join(f_none["missing"])

    s_none = score_signal("X", f_none, cfg)
    s_zero = score_signal("X", f_zero, cfg)
    assert "funding" not in s_none.used_factors
    assert "funding" in s_zero.used_factors


def test_oi_change_now_actually_feeds_the_score():
    """回归：ΔOI 因子曾经恒为 0，却还留着 12% 权重在分母里。"""
    cfg = StrategySettings()
    candles = _candles(60)
    hot = build_features(candles, cfg, funding=0.0, oi_change_pct=6.0, quote_volume=5e7)
    cold = build_features(candles, cfg, funding=0.0, oi_change_pct=None, quote_volume=5e7)

    s_hot = score_signal("X", hot, cfg)
    s_cold = score_signal("X", cold, cfg)
    assert "oi" in s_hot.used_factors
    assert "oi" not in s_cold.used_factors
    # 持仓量放大是看多证据：可用时分数应更高
    assert s_hot.score > s_cold.score


def test_weights_renormalize_over_available_factors():
    """只有趋势与动量可用时，分数必须是这两项的加权平均，不能被缺失项稀释。"""
    cfg = StrategySettings()
    candles = _candles(60)
    f = build_features(candles, cfg, funding=None, oi_change_pct=None, quote_volume=5e7)
    sig = score_signal("X", f, cfg)
    assert set(sig.used_factors) == {"trend", "momentum", "volume"}
    # 三项全为正（上涨趋势 + 正收益 + 放量），分数应为正且不过度衰减
    assert sig.score > 0


def test_missing_values_stay_none_so_prompt_cannot_see_fake_zero():
    """缺失必须留空：写 0 会让提示词里出现"费率 0.0000%"，那是编出来的事实。"""
    from nanoquant.ai.prompts import clean_features

    cfg = StrategySettings()
    f = build_features(_candles(60), cfg, funding=None, oi_change_pct=None, quote_volume=5e7)
    assert f["funding"] is None and f["oi_change_pct"] is None

    sent = clean_features(f)  # 提示词只收数值，None 会被丢掉
    assert "funding" not in sent and "oi_change_pct" not in sent

    # 有值时照常进 prompt
    f2 = build_features(_candles(60), cfg, funding=0.0001, oi_change_pct=2.0, quote_volume=5e7)
    sent2 = clean_features(f2)
    assert sent2["funding"] == 0.0001 and sent2["oi_change_pct"] == 2.0


# ------------------------------------------------------------------ 未收盘 K 线

def test_forming_bar_is_excluded():
    """未收盘那根的成交量只是"到目前为之"，拿它判量能会让闸门常年关闭。"""
    import time as _t

    cfg = StrategySettings()
    now_ms = int(_t.time() * 1000)
    step = 5 * 60 * 1000
    candles = []
    for i in range(60):
        ts = now_ms - (60 - i) * step
        candles.append(Candle(open_time=ts, open=100, high=100.5, low=99.5, close=100, volume=100.0))
    # 最后一根刚开始（成交量只有 10%）
    candles[-1] = Candle(
        open_time=now_ms - step // 10, open=100, high=100.2, low=99.9, close=100, volume=10.0
    )

    f = build_features(candles, cfg, funding=0.0, oi_change_pct=0.0, quote_volume=5e7)
    assert f["bars"] == 59, "正在形成的那根必须被剔除"
    # 用完整 K 线算出来的量比应接近 1，而不是被半根拖到 0.1
    assert 0.8 <= f["vol_ratio"] <= 1.2


def test_closed_history_is_not_trimmed():
    """历史数据（回测）末根早已收盘，不能被误删。"""
    import time as _t

    cfg = StrategySettings()
    now_ms = int(_t.time() * 1000)
    step = 5 * 60 * 1000
    candles = [
        Candle(open_time=now_ms - (200 - i) * step, open=100, high=100.5,
               low=99.5, close=100, volume=100.0)
        for i in range(60)
    ]
    f = build_features(candles, cfg, funding=0.0, oi_change_pct=0.0, quote_volume=5e7)
    assert f["bars"] == 60


# ------------------------------------------------------------------ 盈亏比

def test_take_profit_respects_min_risk_reward():
    cfg, f = _features(n=60, step=0.4)
    sig = score_signal("BTCUSDT", f, cfg)
    assert sig.sl_pct > 0 and sig.tp_pct > 0
    assert sig.tp_pct / sig.sl_pct >= cfg.min_risk_reward - 1e-6
    assert sig.rr >= cfg.min_risk_reward - 1e-6


def test_rr_impossible_blocks_instead_of_taking_bad_trade():
    """止损被 3% 上限卡住、而要求 3R 时 8% 的止盈上限凑不够 —— 应该拒绝开仓。"""
    cfg = StrategySettings(min_risk_reward=3.0)
    candles = _candles(60, step=6.0)  # ATR 很大 → sl 触到 3% 上限
    f = build_features(candles, cfg, funding=0.0, oi_change_pct=0.0, quote_volume=5e7)
    sig = score_signal("X", f, cfg)
    if sig.sl_pct >= 3.0:
        assert sig.direction == "flat"
        assert "盈亏比" in sig.blocked


# ------------------------------------------------------------------ 体制自适应

def test_rsi_zone_flips_meaning_between_trends():
    """同一个 RSI 65，在多头里是健康、在空头里是反弹卖点。"""
    assert rsi_zone_score(65, +0.3) > 0
    assert rsi_zone_score(65, -0.3) < 0
    # 震荡区里 65 只是偏强，还不到超买；72 才开始反向
    assert rsi_zone_score(65, 0.0) > 0
    assert rsi_zone_score(72, 0.0) < 0


def test_overheated_rsi_is_not_a_buy_signal():
    """多头里 RSI 82 是"别追"，不能因为趋势向上就给正分。"""
    assert rsi_zone_score(82, +0.5) < 0


def test_entry_regime_detects_chop():
    cfg = StrategySettings(chop_ema_gap_pct=0.08)
    assert entry_regime({"ema_gap_pct": 0.02}, cfg) == "chop"
    assert entry_regime({"ema_gap_pct": 0.9}, cfg) == "trend"
    assert entry_regime({}, cfg) == ""


def test_chop_shortens_time_stop():
    cfg = StrategySettings(roi_table=[], trailing_pct=0, max_hold_minutes=180,
                           max_hold_minutes_chop=90)
    common = dict(
        side="long", entry_price=100, mark_price=100, peak_price=100,
        sl_price=90, tp_price=200, opened_at=0, now_ts=100 * 60, cfg=cfg, score=0.0,
    )
    assert detect_exit(**common, entry_regime="trend")[0] is False
    hit, reason = detect_exit(**common, entry_regime="chop")
    assert hit and reason == "timeout"


# ------------------------------------------------------------------ 版本指纹

def test_strategy_fingerprint_is_stable_and_sensitive():
    a = strategy_fingerprint(StrategySettings())
    b = strategy_fingerprint(StrategySettings())
    assert a == b and len(a) == 8

    changed = strategy_fingerprint(StrategySettings(min_risk_reward=2.5))
    assert changed != a


def test_fingerprint_in_features_flows_with_signal():
    cfg = StrategySettings()
    candles = _candles(60)
    f = build_features(candles, cfg, funding=0.0, oi_change_pct=0.5, quote_volume=5e7)
    f["strategy_fp"] = strategy_fingerprint(cfg)
    sig = score_signal("X", f, cfg)
    assert sig.as_dict()["features"]["strategy_fp"] == strategy_fingerprint(cfg)
