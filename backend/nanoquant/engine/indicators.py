"""轻量技术指标 (纯 Python, 无 numpy 依赖, 便于离线部署)."""
from __future__ import annotations

import math
from typing import Any

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


def adx(candles: list[Candle], period: int = 14) -> float:
    """Wilder 平均趋向指数 (ADX)。衡量趋势强度，与方向无关：>25 才有趋势。

    输入不足 ``period*2+1`` 根时返回 0.0 —— 0 不表示"无趋势"，而是"算不出"，
    调用方必须区分（见 strategy 里的缺失语义）。
    """
    n = len(candles)
    if n < period * 2 + 1:
        return 0.0
    tr: list[float] = []
    pdm: list[float] = []
    ndm: list[float] = []
    for i in range(1, n):
        c, p = candles[i], candles[i - 1]
        up = c.high - p.high
        dn = p.low - c.low
        pdm.append(up if (up > dn and up > 0) else 0.0)
        ndm.append(dn if (dn > up and dn > 0) else 0.0)
        tr.append(max(c.high - c.low, abs(c.high - p.close), abs(c.low - p.close)))

    def wilder(series: list[float]) -> list[float]:
        smoothed = sum(series[:period])
        out = [smoothed]
        for v in series[period:]:
            smoothed = smoothed - smoothed / period + v
            out.append(smoothed)
        return out

    atr_s = wilder(tr)
    pdi_s = wilder(pdm)
    mdi_s = wilder(ndm)
    if not atr_s or atr_s[-1] <= 0:
        return 0.0
    dx: list[float] = []
    # 变量别复用 p/m: 上面 p 已经是 Candle, 同名会让人误以为这里也是价格对象
    for _a, plus_dm, minus_dm in zip(atr_s, pdi_s, mdi_s):
        s = plus_dm + minus_dm
        dx.append(100.0 * abs(plus_dm - minus_dm) / s if s > 0 else 0.0)
    # ADX = 对 DX 再做一次 Wilder 平滑
    if len(dx) < period + 1:
        return 0.0
    adx_sm = sum(dx[:period])
    for v in dx[period:]:
        adx_sm = adx_sm - adx_sm / period + v
    return adx_sm / period


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


class IncrementalIndicators:
    """回测递推指标。EMA、MACD、ADX 每根只吃上一根的状态。

    RSI、ATR、收益、量比仍只看各自周期那么多根。预热段先 ``update`` 再进主循环，
    这样主循环里的第一根和「从这段行情开头递推」是同一条 EMA，不会每根重算整窗。
    """

    def __init__(self, cfg: Any) -> None:
        self.ema_fast_p = max(1, int(cfg.ema_fast))
        self.ema_slow_p = max(1, int(cfg.ema_slow))
        self.rsi_p = max(1, int(cfg.rsi_period))
        self.atr_p = max(1, int(cfg.atr_period))
        self.adx_p = max(1, int(cfg.adx_period))
        self.mr_p = max(2, int(cfg.mr_window))
        self._k_fast = 2.0 / (self.ema_fast_p + 1.0)
        self._k_slow = 2.0 / (self.ema_slow_p + 1.0)
        self._kf = 2.0 / 13.0
        self._ks = 2.0 / 27.0
        self._kg = 2.0 / 10.0
        self.n = 0
        self.ema_fast = 0.0
        self.ema_slow = 0.0
        self._mf = 0.0
        self._ms = 0.0
        self._macd_sig = 0.0
        self.macd_hist = 0.0
        self.adx = 0.0
        self._prev_close = 0.0
        self._prev_high = 0.0
        self._prev_low = 0.0
        self._diffs: list[float] = []
        self._trs: list[float] = []
        self._closes: list[float] = []
        self._highs: list[float] = []
        self._lows: list[float] = []
        self._vols: list[float] = []
        self._adx_tr = self._adx_pdm = self._adx_ndm = 0.0
        self._adx_n = 0
        self._dx: list[float] = []
        self._adx_sm = 0.0
        self._adx_ready = False

    def update(self, candle: Any) -> None:
        close = float(candle.close)
        high = float(candle.high)
        low = float(candle.low)
        self.n += 1
        if self.n == 1:
            self.ema_fast = self.ema_slow = self._mf = self._ms = close
            self._prev_close, self._prev_high, self._prev_low = close, high, low
        else:
            self.ema_fast = close * self._k_fast + self.ema_fast * (1 - self._k_fast)
            self.ema_slow = close * self._k_slow + self.ema_slow * (1 - self._k_slow)
            self._mf = close * self._kf + self._mf * (1 - self._kf)
            self._ms = close * self._ks + self._ms * (1 - self._ks)
            diff = self._mf - self._ms
            if self.n == 2:
                self._macd_sig = diff
            else:
                self._macd_sig = diff * self._kg + self._macd_sig * (1 - self._kg)
            self.macd_hist = diff - self._macd_sig if self.n >= 35 else 0.0
            self._diffs.append(close - self._prev_close)
            if len(self._diffs) > self.rsi_p:
                del self._diffs[0]
            tr = max(high - low, abs(high - self._prev_close), abs(low - self._prev_close))
            self._trs.append(tr)
            if len(self._trs) > self.atr_p:
                del self._trs[0]
            up = high - self._prev_high
            dn = self._prev_low - low
            pdm = up if (up > dn and up > 0) else 0.0
            ndm = dn if (dn > up and dn > 0) else 0.0
            self.adx = self._step_adx(tr, pdm, ndm)
            self._prev_close, self._prev_high, self._prev_low = close, high, low
        self._closes.append(close)
        self._highs.append(high)
        self._lows.append(low)
        self._vols.append(float(candle.volume))
        keep = max(self.mr_p, 21, 16)
        if len(self._closes) > keep:
            del self._closes[0]
            del self._highs[0]
            del self._lows[0]
            del self._vols[0]

    def _step_adx(self, tr: float, pdm: float, ndm: float) -> float:
        p = self.adx_p
        self._adx_n += 1
        if self._adx_n <= p:
            self._adx_tr += tr
            self._adx_pdm += pdm
            self._adx_ndm += ndm
            if self._adx_n < p:
                return 0.0
        else:
            self._adx_tr = self._adx_tr - self._adx_tr / p + tr
            self._adx_pdm = self._adx_pdm - self._adx_pdm / p + pdm
            self._adx_ndm = self._adx_ndm - self._adx_ndm / p + ndm
        if self._adx_tr <= 0:
            return 0.0
        s = self._adx_pdm + self._adx_ndm
        dx = 100.0 * abs(self._adx_pdm - self._adx_ndm) / s if s > 0 else 0.0
        if not self._adx_ready:
            self._dx.append(dx)
            if len(self._dx) < p:
                return 0.0
            self._adx_sm = sum(self._dx)
            self._dx.clear()
            self._adx_ready = True
            return self._adx_sm / p
        self._adx_sm = self._adx_sm - self._adx_sm / p + dx
        return self._adx_sm / p

    def features(self, quote_volume: float) -> dict[str, Any]:
        closes = self._closes
        price = closes[-1] if closes else 1.0
        price = price or 1.0
        bars = self.n
        gains = sum(d for d in self._diffs if d > 0)
        losses = -sum(d for d in self._diffs if d < 0)
        if not self._diffs or losses == 0:
            rsi_val = 100.0 if gains > 0 else 50.0
        else:
            rs = (gains / self.rsi_p) / (losses / self.rsi_p)
            rsi_val = 100.0 - 100.0 / (1.0 + rs)
        atr_val = sum(self._trs) / len(self._trs) if self._trs else 0.0
        z, mid, sd = zscore_last(closes, self.mr_p)
        vols = self._vols
        avg_vol = sum(vols[-20:]) / max(1, len(vols[-20:])) if vols else 0.0
        return {
            "bars": bars,
            "close": round(price, 8),
            "ema_fast": round(self.ema_fast, 8),
            "ema_slow": round(self.ema_slow, 8),
            "rsi": rsi_val,
            "atr": round(atr_val, 8),
            "macd_hist": self.macd_hist,
            "ret_1": pct_change(closes, 1),
            "ret_5": pct_change(closes, 5),
            "ret_15": pct_change(closes, 15),
            "vol_ratio": round(vols[-1] / avg_vol, 4) if avg_vol > 0 else 0.0,
            "funding": None,
            "open_interest": 0.0,
            "oi_change_pct": None,
            "change_24h": 0.0,
            "quote_volume": quote_volume,
            "data_degraded": False,
            "data_gaps": 0,
            "data_age_sec": None,
            "data_ok": True,
            "data_note": "",
            "htf_trend": "",
            "atr_pct": round(atr_val / price * 100, 4),
            "ema_gap_pct": round((self.ema_fast - self.ema_slow) / price * 100, 4),
            "range_pct": round((self._highs[-1] - self._lows[-1]) / price * 100, 4) if self._highs else 0.0,
            "band_z": round(z, 4),
            "band_mid": round(mid, 8),
            "band_sd_pct": round(sd / price * 100, 4),
            "adx": round(self.adx, 4),
            "ok": {
                "trend": bars >= self.ema_slow_p + 1,
                "momentum": bars >= max(self.rsi_p + 1, 35, 16),
                "volume": bars >= 21,
                "funding": False,
                "oi": False,
                "meanrev": bars >= self.mr_p,
                "adx": bars >= self.adx_p * 2 + 1,
            },
            "missing": [],
        }


def clamp(value: float, low: float = -1.0, high: float = 1.0) -> float:
    return max(low, min(high, value))


def tanh_norm(value: float, scale: float) -> float:
    if scale <= 0:
        return 0.0
    return math.tanh(value / scale)
