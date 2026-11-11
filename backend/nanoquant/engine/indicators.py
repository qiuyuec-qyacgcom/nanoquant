"""轻量技术指标 (纯 Python, 无 numpy 依赖, 便于离线部署)."""
from __future__ import annotations

import math

from ..exchange.client import Candle


def ema(values: list[float], period: int) -> float:
    if not values:
        return 0.0
    if len(values) < period:
        period = max(1, len(values))
    k = 2.0 / (period + 1.0)
    out = values[0]
    for v in values[1:]:
        out = v * k + out * (1 - k)
    return out


def ema_series(values: list[float], period: int) -> list[float]:
    if not values:
        return []
    k = 2.0 / (period + 1.0)
    out = [values[0]]
    for v in values[1:]:
        out.append(v * k + out[-1] * (1 - k))
    return out


def rsi(values: list[float], period: int = 14) -> float:
    if len(values) < 2:
        return 50.0
    window = values[-(period + 1) :]
    gains = 0.0
    losses = 0.0
    for prev, cur in zip(window, window[1:]):
        diff = cur - prev
        if diff >= 0:
            gains += diff
        else:
            losses -= diff
    if losses == 0:
        return 100.0 if gains > 0 else 50.0
    rs = (gains / period) / (losses / period)
    return 100.0 - 100.0 / (1.0 + rs)


def atr(candles: list[Candle], period: int = 14) -> float:
    if len(candles) < 2:
        return 0.0
    window = candles[-(period + 1) :]
    trs: list[float] = []
    for prev, cur in zip(window, window[1:]):
        trs.append(max(cur.high - cur.low, abs(cur.high - prev.close), abs(cur.low - prev.close)))
    return sum(trs) / len(trs) if trs else 0.0


def macd_hist(values: list[float], fast: int = 12, slow: int = 26, signal: int = 9) -> float:
    if len(values) < slow + signal:
        return 0.0
    e_fast = ema_series(values, fast)
    e_slow = ema_series(values, slow)
    diff = [f - s for f, s in zip(e_fast, e_slow)]
    sig = ema_series(diff, signal)
    return diff[-1] - sig[-1]


def stdev(values: list[float]) -> float:
    if len(values) < 2:
        return 0.0
    mean = sum(values) / len(values)
    var = sum((v - mean) ** 2 for v in values) / (len(values) - 1)
    return math.sqrt(max(0.0, var))


def zscore_last(values: list[float], period: int) -> tuple[float, float, float]:
    """最后一根相对近 period 根的标准分：(z, 均值, 标准差)。

    z 用**样本标准差**，并在标准差为 0（横盘到没有波动）时返回 z=0 ——
    否则会出现除零或"无穷大的偏离"，那是数据退化不是信号。
    """
    if len(values) < max(3, period) or period <= 1:
        return 0.0, 0.0, 0.0
    window = values[-period:]
    mean = sum(window) / len(window)
    sd = stdev(window)
    if sd <= 0:
        return 0.0, mean, 0.0
    return (window[-1] - mean) / sd, mean, sd


def pct_change(values: list[float], bars: int) -> float:
    if len(values) <= bars or values[-bars - 1] == 0:
        return 0.0
    return (values[-1] / values[-bars - 1] - 1) * 100


def clamp(value: float, low: float = -1.0, high: float = 1.0) -> float:
    return max(low, min(high, value))


def tanh_norm(value: float, scale: float) -> float:
    if scale <= 0:
        return 0.0
    return math.tanh(value / scale)
