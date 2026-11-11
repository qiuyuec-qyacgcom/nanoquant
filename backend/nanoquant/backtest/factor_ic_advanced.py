# -*- coding: utf-8 -*-
"""评估"换信息源"的新因子 IC：资金费率分位 / CVD 订单流不平衡。

动机：OHLCV 五因子（EMA/RSI/动量/量能/ADX）的 IC 实测只有 0.04~0.05，
被往返成本吃掉，任何周期样本外都是负的。要"救"，只能换 OHLCV 里没有的
**领先且私有**的信息。这里挑两个可公开拉取的候选，先量一量它们的预测力：

1. `funding_percentile`：当前资金费率在过去 N 期的分位数。
   资金费率极端（多头/空头拥挤）是公认的反向信号，理论上 IC 应为负且更强。
2. `cvd`：主动买卖不平衡（Cumulative Volume Delta）。大额主动买单 = 聪明钱进场。

数据：4h K 线 + 资金费率历史（8h 结算，公开接口，无需凭证）。
结论只看 IC：能不能突破 0.10 的"可交易"门槛。

用法:
  python -m nanoquant.backtest.factor_ic_advanced --symbols BTCUSDT,ETHUSDT --funding
  python -m nanoquant.backtest.factor_ic_advanced --symbols BTCUSDT --cvd
"""
from __future__ import annotations

import bisect
import sys
import time
from typing import Any

import httpx

from ..exchange.client import Candle

FAPI = "https://fapi.binance.com/fapi/v1"


# ------------------------------------------------------------------ 取数

def _load_klines(symbol: str, interval: str, bars: int) -> list[Candle]:
    out: list[Candle] = []
    end_ms: int | None = None
    per = 1000
    while len(out) < bars:
        params: dict[str, Any] = {"symbol": symbol, "interval": interval, "limit": per}
        if end_ms is not None:
            params["endTime"] = end_ms
        resp = httpx.get(f"{FAPI}/klines", params=params, timeout=30)
        resp.raise_for_status()
        rows = resp.json()
        if not isinstance(rows, list) or not rows:
            break
        batch = [
            Candle(open_time=int(k[0]), open=float(k[1]), high=float(k[2]),
                   low=float(k[3]), close=float(k[4]), volume=float(k[5]))
            for k in rows
        ]
        out = batch + out
        end_ms = int(rows[0][0]) - 1
        if len(rows) < per:
            break
    return out[-bars:]


def _load_funding(symbol: str, limit: int = 1000) -> list[tuple[int, float]]:
    """资金费率历史，返回 [(fundingTime_ms, rate), ...] 按时间升序。"""
    out: list[tuple[int, float]] = []
    start_ms: int | None = None
    per = 1000
    while len(out) < limit:
        params: dict[str, Any] = {"symbol": symbol, "limit": per}
        if start_ms is not None:
            params["startTime"] = start_ms
        resp = httpx.get(f"{FAPI}/fundingRate", params=params, timeout=30)
        resp.raise_for_status()
        rows = resp.json()
        if not isinstance(rows, list) or not rows:
            break
        batch = [(int(r["fundingTime"]), float(r["fundingRate"])) for r in rows]
        out.extend(batch)
        if len(rows) < per:
            break
        start_ms = rows[-1]["fundingTime"] + 1
    out.sort(key=lambda x: x[0])
    return out[-limit:]


def _percentile(history: list[float], value: float) -> float:
    """value 在 history 里的分位（0~1），history 为升序或无序均可。"""
    if not history:
        return 0.5
    below = sum(1 for x in history if x < value)
    return below / len(history)


# ------------------------------------------------------------------ IC

def _spearman(x: list[float], y: list[float]) -> float:
    n = len(x)
    if n < 3:
        return 0.0

    def rank(vals: list[float]) -> list[float]:
        order = sorted(range(len(vals)), key=lambda i: vals[i])
        out = [0.0] * len(vals)
        i = 0
        while i < len(order):
            j = i
            while j + 1 < len(order) and vals[order[j + 1]] == vals[order[i]]:
                j += 1
            avg = (i + j) / 2.0 + 1.0
            for k in range(i, j + 1):
                out[order[k]] = avg
            i = j + 1
        return out

    rx, ry = rank(x), rank(y)
    mx = sum(rx) / n
    my = sum(ry) / n
    cov = sum((rx[i] - mx) * (ry[i] - my) for i in range(n))
    vx = sum((v - mx) ** 2 for v in rx)
    vy = sum((v - my) ** 2 for v in ry)
    denom = (vx * vy) ** 0.5
    return cov / denom if denom > 0 else 0.0


def _hit_rate(x: list[float], y: list[float]) -> float:
    n = len(x)
    if n == 0:
        return 0.0
    return sum(1 for a, b in zip(x, y) if (a > 0) == (b > 0) or a == 0 or b == 0) / n


# ------------------------------------------------------------------ 资金费率分位因子

def evaluate_funding(
    symbol: str,
    *,
    interval: str = "4h",
    bars: int = 2000,
    lookback: int = 90,
    forwards: tuple[int, ...] = (1, 3, 6),
) -> dict[str, Any]:
    """资金费率分位因子对"未来 N 根收益"的 IC。"""
    candles = _load_klines(symbol, interval, bars)
    funding = _load_funding(symbol, max(1000, lookback + bars // 2))
    f_times = [t for t, _ in funding]
    f_rates = [r for _, r in funding]

    series: dict[str, list[float]] = {
        "funding_raw": [],
        "funding_pct": [],
    }
    fwd: dict[int, list[float]] = {k: [] for k in forwards}

    warmup = max(lookback, 1)
    for i in range(1, len(candles) - max(forwards)):
        ts = int(candles[i].open_time)
        # 该时刻之前最近一次已结算的资金费率（避免未来函数）
        j = bisect.bisect_right(f_times, ts) - 1
        if j < warmup:
            continue
        cur = f_rates[j]
        hist = f_rates[max(0, j - lookback):j]
        pct = _percentile(hist, cur)
        series["funding_raw"].append(cur)
        series["funding_pct"].append(pct)
        close = candles[i].close
        for fwd_bars in forwards:
            fwd[fwd_bars].append((candles[i + fwd_bars].close - close) / close * 100)

    rows: list[dict[str, Any]] = []
    for name in ("funding_raw", "funding_pct"):
        for fwd_bars in forwards:
            ic = _spearman(series[name], fwd[fwd_bars])
            rows.append({
                "factor": name,
                "forward_bars": fwd_bars,
                "ic": round(ic, 4),
                "hit_rate": round(_hit_rate(series[name], fwd[fwd_bars]) * 100, 1),
                "n": len(fwd[fwd_bars]),
            })
    return {"symbol": symbol, "interval": interval, "lookback": lookback,
            "samples": len(fwd[forwards[0]]), "rows": rows}


# ------------------------------------------------------------------ CVD 订单流因子

def evaluate_cvd(
    symbol: str,
    *,
    interval: str = "4h",
    bars: int = 500,
    lookback: int = 24,
    forwards: tuple[int, ...] = (1, 3, 6),
) -> dict[str, Any]:
    """CVD 订单流因子：过去 lookback 根 K 线内的主动买卖不平衡。

    用 aggTrades 按 K 线窗口聚合：每笔 m=True 是买方主动（taker buy）。
    CVD = (主动买量 - 主动卖量) / 总成交量，正 = 买压占优。
    """
    candles = _load_klines(symbol, interval, bars)
    # 拉最近一段时间的 aggTrades，按 K 线时间戳分桶
    # （量大，这里只拉 bars 根 K 线覆盖的区间，按 4h 分桶）
    earliest = int(candles[0].open_time)
    taker_buy: dict[int, float] = {}
    taker_sell: dict[int, float] = {}
    end_ms: int | None = None
    per = 1000
    # aggTrades 分页：从最新往前拉，直到覆盖 earliest。
    # 接口限速较严（约 60 req/min），逐页限速 + 429 退避重试。
    while end_ms is None or end_ms > earliest:
        params: dict[str, Any] = {"symbol": symbol, "limit": per}
        if end_ms is not None:
            params["endTime"] = end_ms
        for attempt in range(4):
            resp = httpx.get(f"{FAPI}/aggTrades", params=params, timeout=30)
            if resp.status_code == 429:
                time.sleep(2.0 * (attempt + 1))
                continue
            break
        if resp.status_code == 429:
            break
        resp.raise_for_status()
        rows = resp.json()
        if not isinstance(rows, list) or not rows:
            break
        for r in rows:
            t = int(r["T"])
            bucket = t - (t % (4 * 3600 * 1000))  # 按 4h 分桶
            qty = float(r["q"])
            if r["m"]:
                taker_buy[bucket] = taker_buy.get(bucket, 0.0) + qty
            else:
                taker_sell[bucket] = taker_sell.get(bucket, 0.0) + qty
        end_ms = int(rows[0]["T"]) - 1
        if len(rows) < per:
            break
        time.sleep(0.25)

    # 每根 K 线：过去 lookback 个 4h 桶的 CVD
    series: list[float] = []
    fwd: dict[int, list[float]] = {k: [] for k in forwards}
    for i in range(lookback, len(candles) - max(forwards)):
        buy = sell = 0.0
        for k in range(lookback):
            bucket = int(candles[i - k].open_time)
            buy += taker_buy.get(bucket, 0.0)
            sell += taker_sell.get(bucket, 0.0)
        total = buy + sell
        cvd = (buy - sell) / total if total > 0 else 0.0
        series.append(cvd)
        close = candles[i].close
        for fwd_bars in forwards:
            fwd[fwd_bars].append((candles[i + fwd_bars].close - close) / close * 100)

    rows = []
    for fwd_bars in forwards:
        ic = _spearman(series, fwd[fwd_bars])
        rows.append({
            "factor": "cvd",
            "forward_bars": fwd_bars,
            "ic": round(ic, 4),
            "hit_rate": round(_hit_rate(series, fwd[fwd_bars]) * 100, 1),
            "n": len(fwd[fwd_bars]),
        })
    return {"symbol": symbol, "interval": interval, "lookback": lookback,
            "samples": len(series), "rows": rows}


# ------------------------------------------------------------------ CLI

def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description="评估换信息源新因子的 IC")
    parser.add_argument("--symbols", default="BTCUSDT,ETHUSDT,SOLUSDT")
    parser.add_argument("--funding", action="store_true", help="资金费率分位因子")
    parser.add_argument("--cvd", action="store_true", help="CVD 订单流因子")
    parser.add_argument("--interval", default="4h")
    parser.add_argument("--bars", type=int, default=2000)
    args = parser.parse_args(argv)

    if not args.funding and not args.cvd:
        args.funding = True  # 默认跑资金费率

    symbols = [s.strip() for s in args.symbols.split(",") if s.strip()]

    if args.funding:
        print(f"{'标的':10} {'因子':14} {'前瞻':>4} {'IC':>8} {'命中率':>8} {'样本':>6}  判定")
        print("-" * 78)
        for sym in symbols:
            try:
                r = evaluate_funding(sym, interval=args.interval, bars=args.bars)
            except Exception as exc:  # noqa: BLE001
                print(f"{sym:10} 失败 {exc}")
                continue
            for row in r["rows"]:
                ic = row["ic"]
                verdict = ("强预测力" if abs(ic) >= 0.10 else
                           "中等" if abs(ic) >= 0.05 else "弱")
                print(f"{sym:10} {row['factor']:14} {row['forward_bars']:>4} "
                      f"{ic:>8.4f} {row['hit_rate']:>7.1f}% {row['n']:>6}  {verdict}")

    if args.cvd:
        print(f"\n{'标的':10} {'因子':14} {'前瞻':>4} {'IC':>8} {'命中率':>8} {'样本':>6}  判定")
        print("-" * 78)
        for sym in symbols:
            try:
                r = evaluate_cvd(sym, interval=args.interval, bars=min(args.bars, 500))
            except Exception as exc:  # noqa: BLE001
                print(f"{sym:10} 失败 {exc}")
                continue
            for row in r["rows"]:
                ic = row["ic"]
                verdict = ("强预测力" if abs(ic) >= 0.10 else
                           "中等" if abs(ic) >= 0.05 else "弱")
                print(f"{sym:10} {row['factor']:14} {row['forward_bars']:>4} "
                      f"{ic:>8.4f} {row['hit_rate']:>7.1f}% {row['n']:>6}  {verdict}")
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
