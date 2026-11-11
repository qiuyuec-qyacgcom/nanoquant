# -*- coding: utf-8 -*-
"""因子 IC 评估：每个因子对"未来 N 根收益"的预测能力。

用途：回答"这 5 个因子到底哪个有 edge、哪个在拖后腿"。
IC（信息系数）= 因子值与未来收益的秩相关，范围 [-1, 1]：
  - |IC| 接近 0：该因子对方向没有预测力；
  - IC 为正且稳定：因子值越大，未来涨得越多（趋势因子该有的样子）；
  - IC 为负：因子在反向（可以考虑取反，或干脆停用）。

配合命中率（符号一致的比例）一起看，避免只盯一个数字。
本脚本只读行情，不碰账户，可离线跑。

用法:
  python -m nanoquant.backtest.factor_ic --symbols BTCUSDT,ETHUSDT --interval 15m --bars 3000
"""
from __future__ import annotations

import sys
import time
from typing import Any

from ..engine.strategy import build_features
from ..services.settings import StrategySettings


def _load_klines(symbol: str, interval: str, bars: int) -> list[Any]:
    """分页取 K 线，突破单次 1500 根上限，用于拉长样本。"""
    import httpx

    from ..exchange.client import Candle

    out: list[Candle] = []
    end_ms: int | None = None
    per = 1000
    while len(out) < bars:
        params: dict[str, Any] = {"symbol": symbol, "interval": interval, "limit": per}
        if end_ms is not None:
            params["endTime"] = end_ms
        resp = httpx.get("https://fapi.binance.com/fapi/v1/klines", params=params, timeout=30)
        resp.raise_for_status()
        rows = resp.json()
        if not isinstance(rows, list) or not rows:
            break
        batch = [
            Candle(
                open_time=int(k[0]), open=float(k[1]), high=float(k[2]),
                low=float(k[3]), close=float(k[4]), volume=float(k[5]),
            )
            for k in rows
        ]
        out = batch + out
        end_ms = int(rows[0][0]) - 1  # 取更早的区间
        if len(rows) < per:
            break
    return out[-bars:]


def _rank(values: list[float]) -> list[float]:
    order = sorted(range(len(values)), key=lambda i: values[i])
    ranks = [0.0] * len(values)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and values[order[j + 1]] == values[order[i]]:
            j += 1
        avg = (i + j) / 2.0 + 1.0
        for k in range(i, j + 1):
            ranks[order[k]] = avg
        i = j + 1
    return ranks


def spearman(x: list[float], y: list[float]) -> float:
    """Spearman 秩相关。输入等长。"""
    n = len(x)
    if n < 3:
        return 0.0
    rx, ry = _rank(x), _rank(y)
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
    hits = sum(1 for a, b in zip(x, y) if (a > 0) == (b > 0) or a == 0 or b == 0)
    return hits / n


#: 参与评估的因子（价格/成交量能算出来的；资金费率与 OI 无历史序列，不在此列）
_FACTORS = {
    "ema_gap_pct": "EMA 结构(趋势)",
    "rsi": "RSI",
    "ret_5": "5 根收益(动量)",
    "vol_ratio": "量比",
    "adx": "ADX(趋势强度)",
}


def evaluate(symbol: str, interval: str, bars: int, forward: int) -> dict[str, Any]:
    candles = _load_klines(symbol, interval, bars)
    if len(candles) < 120:
        return {"error": f"{symbol} 数据不足 {len(candles)} 根"}
    cfg = StrategySettings(timeframe=interval)
    series: dict[str, list[float]] = {k: [] for k in _FACTORS}
    fwd: list[float] = []
    # 只需最近 warmup 根就能算全所有指标（EMA/RSI/ATR/ADX/z-score/量比）。
    # 用固定窗口而非全量切片，把 O(n²) 降到 O(n·warmup)，长样本（数万根）才跑得动。
    warmup = 60
    for i in range(warmup, len(candles) - forward):
        window = candles[i + 1 - warmup : i + 1]
        f = build_features(window, cfg, funding=None, oi_change_pct=None, quote_volume=8e7)
        close = f["close"] or 0.0
        if close <= 0:
            continue
        future = candles[i + forward].close
        fwd.append((future - close) / close * 100)
        for key in _FACTORS:
            series[key].append(float(f.get(key) or 0.0))

    rows: list[dict[str, Any]] = []
    for key, label in _FACTORS.items():
        ic = spearman(series[key], fwd)
        rows.append(
            {
                "factor": key,
                "label": label,
                "ic": round(ic, 4),
                "hit_rate": round(_hit_rate(series[key], fwd) * 100, 1),
                "n": len(fwd),
            }
        )
    rows.sort(key=lambda r: -abs(r["ic"]))
    return {"symbol": symbol, "interval": interval, "forward_bars": forward,
            "samples": len(fwd), "factors": rows}


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description="因子 IC 评估")
    parser.add_argument("--symbols", default="BTCUSDT,ETHUSDT,SOLUSDT")
    parser.add_argument("--interval", default="15m")
    parser.add_argument("--bars", type=int, default=3000, help="K 线根数（分页取，可 >1500）")
    parser.add_argument("--forward", type=int, default=3, help="预测未来 N 根")
    args = parser.parse_args(argv)

    header = f"{'因子':14} {'说明':18} {'IC':>8} {'命中率':>8} {'样本':>7}  判定"
    print(header)
    print("-" * 78)
    for raw in [s.strip() for s in args.symbols.split(",") if s.strip()]:
        try:
            result = evaluate(raw, args.interval, args.bars, args.forward)
        except Exception as exc:  # noqa: BLE001
            print(f"{raw}: 失败 {exc}")
            continue
        if result.get("error"):
            print(f"{raw}: {result['error']}")
            continue
        print(f"\n{raw} ({result['samples']} 样本, 预测 {result['forward_bars']} 根)")
        for row in result["factors"]:
            ic, hit = row["ic"], row["hit_rate"]
            verdict = (
                "有正向预测力" if ic > 0.03 else
                "负相关(考虑取反)" if ic < -0.03 else "无预测力"
            )
            print(f"  {row['factor']:14} {row['label']:18} {ic:>8.4f} {hit:>7.1f}% {row['n']:>7}  {verdict}")
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
