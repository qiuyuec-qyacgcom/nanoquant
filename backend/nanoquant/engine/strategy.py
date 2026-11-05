"""超短线多因子信号.

五个因子加权合成方向分 [-1, 1]:
  trend(EMA 结构) + momentum(RSI/短周期收益) + volume(量比) + funding(资金费率拥挤度) + oi(持仓量变化)
再叠加波动率/流动性/量能三重准入过滤, 输出可解释的 reasons.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from ..exchange.client import Candle
from ..services.settings import StrategySettings
from . import indicators as ta


@dataclass
class SignalResult:
    symbol: str
    direction: str = "flat"  # long / short / flat
    score: float = 0.0
    features: dict[str, Any] = field(default_factory=dict)
    reasons: list[str] = field(default_factory=list)
    sl_pct: float = 0.0
    tp_pct: float = 0.0
    blocked: str = ""

    def as_dict(self) -> dict[str, Any]:
        return {
            "symbol": self.symbol,
            "direction": self.direction,
            "score": round(self.score, 4),
            "features": self.features,
            "reasons": self.reasons,
            "sl_pct": round(self.sl_pct, 4),
            "tp_pct": round(self.tp_pct, 4),
            "blocked": self.blocked,
        }


def build_features(
    candles: list[Candle],
    cfg: StrategySettings,
    *,
    funding: float = 0.0,
    open_interest: float = 0.0,
    change_24h: float = 0.0,
    quote_volume: float = 0.0,
) -> dict[str, Any]:
    closes = [c.close for c in candles]
    vols = [c.volume for c in candles]
    last = candles[-1] if candles else None
    f = {
        "close": last.close if last else 0.0,
        "ema_fast": ta.ema(closes, cfg.ema_fast),
        "ema_slow": ta.ema(closes, cfg.ema_slow),
        "rsi": ta.rsi(closes, cfg.rsi_period),
        "atr": ta.atr(candles, cfg.atr_period),
        "macd_hist": ta.macd_hist(closes),
        "ret_1": ta.pct_change(closes, 1),
        "ret_5": ta.pct_change(closes, 5),
        "ret_15": ta.pct_change(closes, 15),
        "vol_ratio": 0.0,
        "funding": funding,
        "open_interest": open_interest,
        "change_24h": change_24h,
        "quote_volume": quote_volume,
    }
    avg_vol = sum(vols[-20:]) / max(1, len(vols[-20:])) if vols else 0.0
    f["vol_ratio"] = round(vols[-1] / avg_vol, 4) if avg_vol > 0 else 0.0
    price = f["close"] or 1.0
    f["atr_pct"] = round(f["atr"] / price * 100, 4)
    f["ema_gap_pct"] = round((f["ema_fast"] - f["ema_slow"]) / price * 100, 4)
    f["range_pct"] = round((last.high - last.low) / price * 100, 4) if last else 0.0
    for key in ("close", "ema_fast", "ema_slow", "atr"):
        f[key] = round(f[key], 8)
    return f


def score_signal(symbol: str, features: dict[str, Any], cfg: StrategySettings) -> SignalResult:
    res = SignalResult(symbol=symbol, features=features)
    price = features.get("close") or 0.0
    if price <= 0:
        res.blocked = "行情缺失"
        return res

    trend = ta.tanh_norm(features["ema_gap_pct"], 0.22)
    momentum = ta.clamp(
        0.55 * ta.tanh_norm(features["ret_5"], 0.45)
        + 0.30 * ((features["rsi"] - 50.0) / 50.0)
        + 0.15 * ta.tanh_norm(features["macd_hist"] / price * 100, 0.25)
    )
    volume = ta.clamp((features["vol_ratio"] - 1.0) * 0.9) * (1 if features["ret_1"] >= 0 else -1)
    funding = ta.clamp(-features["funding"] / 0.0008)
    oi = ta.clamp(features.get("oi_change_pct", 0.0) / 3.0)

    w = (cfg.weight_trend, cfg.weight_momentum, cfg.weight_volume, cfg.weight_funding, cfg.weight_oi)
    total_w = sum(w) or 1.0
    score = (trend * w[0] + momentum * w[1] + volume * w[2] + funding * w[3] + oi * w[4]) / total_w
    res.score = round(ta.clamp(score), 4)

    res.reasons = [
        f"EMA 结构 {features['ema_gap_pct']:+.3f}%",
        f"RSI {features['rsi']:.1f}",
        f"1/5 分钟收益 {features['ret_1']:+.3f}% / {features['ret_5']:+.3f}%",
        f"量比 {features['vol_ratio']:.2f}",
        f"资金费率 {features['funding'] * 100:+.4f}%",
        f"ATR {features['atr_pct']:.3f}%",
    ]

    # ---- 准入过滤 (可解释的拒绝原因)
    if features["atr_pct"] < cfg.atr_pct_min:
        res.blocked = f"波动率过低 {features['atr_pct']:.3f}% < {cfg.atr_pct_min}%"
        return res
    if features["atr_pct"] > cfg.atr_pct_max:
        res.blocked = f"波动率过高 {features['atr_pct']:.3f}% > {cfg.atr_pct_max}%"
        return res
    if features["vol_ratio"] < cfg.volume_ratio_min:
        res.blocked = f"量能不足 {features['vol_ratio']:.2f} < {cfg.volume_ratio_min}"
        return res
    if features.get("quote_volume", 0) < cfg.min_24h_quote_volume:
        res.blocked = f"24h 成交额偏低 {features.get('quote_volume', 0) / 1e6:.1f}M"
        return res

    if res.score >= cfg.entry_threshold:
        res.direction = "long"
    elif res.score <= -cfg.entry_threshold:
        res.direction = "short"
    else:
        res.direction = "flat"

    # 结构确认: 趋势方向必须与分数一致, 避免逆势抄底
    if res.direction == "long" and features["ema_fast"] <= features["ema_slow"]:
        res.blocked = "多头信号缺少均线结构确认"
        res.direction = "flat"
    if res.direction == "short" and features["ema_fast"] >= features["ema_slow"]:
        res.blocked = "空头信号缺少均线结构确认"
        res.direction = "flat"

    atr_pct = features["atr_pct"]
    res.sl_pct = round(min(3.0, max(0.18, atr_pct * cfg.sl_atr_mult)), 4)
    res.tp_pct = round(min(8.0, max(res.sl_pct * 1.1, atr_pct * cfg.tp_atr_mult)), 4)
    return res


def partial_tp_r(side: str, entry_price: float, initial_sl: float, mark_price: float) -> float:
    """当前浮盈相当于多少个初始风险 (R).

    R = |开仓价 - 初始止损价|; 用**初始**止损而不是当前 sl_price,
    否则移动止损上移后 R 会失真, 部分止盈永远触发不了。
    """
    # initial_sl <= 0 表示该仓位没有记录初始止损 (老数据), 不能拿 0 当止损算出假 R
    if initial_sl <= 0 or entry_price <= 0:
        return 0.0
    risk = abs(entry_price - initial_sl)
    if risk <= 0:
        return 0.0
    direction = 1 if side == "long" else -1
    return (mark_price - entry_price) * direction / risk


def roi_target(table: list[list[float]], minutes: float) -> float | None:
    """时间阶梯止盈目标 (freqtrade minimal_roi 思路).

    取"分钟数不超过当前持仓时长"的最高一档: 持仓越久, 要求的收益率越低,
    让 1 分钟级别的单子尽快落袋, 而不是拖到超时被行情反噬.
    """
    target: float | None = None
    for row in table or []:
        try:
            minutes_at, pct = float(row[0]), float(row[1])
        except (TypeError, ValueError, IndexError):
            continue
        if minutes >= minutes_at:
            target = pct
        else:
            break
    return target


def detect_exit(
    *,
    side: str,
    entry_price: float,
    mark_price: float,
    peak_price: float,
    sl_price: float,
    tp_price: float,
    opened_at: int,
    now_ts: int,
    cfg: StrategySettings,
    score: float,
) -> tuple[bool, str]:
    """本地出场判断: 硬止损 / 止盈 / 阶梯止盈 / 移动止损 / 信号反转 / 超时."""
    direction = 1 if side == "long" else -1
    if sl_price > 0 and ((direction == 1 and mark_price <= sl_price) or (direction == -1 and mark_price >= sl_price)):
        return True, "stop_loss"
    if tp_price > 0 and ((direction == 1 and mark_price >= tp_price) or (direction == -1 and mark_price <= tp_price)):
        return True, "take_profit"
    # 时间阶梯止盈: 与固定 TP 并存, 先到者先平
    if entry_price > 0 and cfg.roi_table:
        profit_pct = (mark_price - entry_price) * direction / entry_price * 100
        target = roi_target(cfg.roi_table, max(0.0, (now_ts - opened_at) / 60.0))
        if target is not None and target > 0 and profit_pct >= target:
            return True, "roi"
    if cfg.trailing_pct > 0 and peak_price > 0:
        if direction == 1 and mark_price <= peak_price * (1 - cfg.trailing_pct / 100):
            return True, "trailing_stop"
        if direction == -1 and mark_price >= peak_price * (1 + cfg.trailing_pct / 100):
            return True, "trailing_stop"
    if (now_ts - opened_at) >= cfg.max_hold_minutes * 60:
        return True, "timeout"
    if direction == 1 and score <= -cfg.exit_threshold:
        return True, "signal_flip"
    if direction == -1 and score >= cfg.exit_threshold:
        return True, "signal_flip"
    return False, ""
