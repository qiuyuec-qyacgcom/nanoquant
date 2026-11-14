# -*- coding: utf-8 -*-
"""因子实验室：统一的因子 / 信号评估入口。

原先分散的三个工具融合到这里，共享同一套基础件：

- ``factor_ic``          → :func:`evaluate_factors`（OHLCV 五因子 IC）
- ``factor_ic_advanced`` → :func:`evaluate_funding` / :func:`evaluate_cvd`
- ``signal_source_eval`` → :func:`evaluate_signal_sources`（信号源事后命中率）

它们回答的其实是同一个问题：**某个量，对未来的方向有没有预测力**。
区别只在「某个量」从哪来——OHLCV 派生 / 资金费率 / 订单流 / 外部信号源。

共享件：分页取 K 线 / 资金费率历史 / Spearman 秩相关 / 方向命中率 / 分位数。
只读行情与本地库，不碰账户，可离线跑。

用法::

    python -m nanoquant.backtest.factor_lab --mode all
    python -m nanoquant.backtest.factor_lab --mode factors --interval 15m --bars 3000
    python -m nanoquant.backtest.factor_lab --mode funding --interval 4h
    python -m nanoquant.backtest.factor_lab --mode cvd
    python -m nanoquant.backtest.factor_lab --mode signals
"""
from __future__ import annotations

import bisect
import time
from collections import defaultdict
from typing import Any

FAPI = "https://fapi.binance.com/fapi/v1"


# ============================================================ 共享基础件
def load_klines(symbol: str, interval: str, bars: int, end_ms: int | None = None) -> list[Any]:
    """分页取 K 线，突破单次 1500 根上限，用于拉长样本。

    ``end_ms`` 给定时从该时间点往前拉（按历史时点评估时用）。
    """
    import httpx

    from ..exchange.client import Candle

    out: list[Any] = []
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
            Candle(
                open_time=int(k[0]), open=float(k[1]), high=float(k[2]),
                low=float(k[3]), close=float(k[4]), volume=float(k[5]),
            )
            for k in rows
        ]
        out = batch + out
        end_ms = int(rows[0][0]) - 1
        if len(rows) < per:
            break
    return out[-bars:]


def load_funding(symbol: str, limit: int = 1000) -> list[tuple[int, float]]:
    """资金费率历史，返回 [(fundingTime_ms, rate), ...] 按时间升序。"""
    import httpx

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
        out.extend((int(r["fundingTime"]), float(r["fundingRate"])) for r in rows)
        if len(rows) < per:
            break
        start_ms = rows[-1]["fundingTime"] + 1
    out.sort(key=lambda x: x[0])
    return out[-limit:]


def percentile(history: list[float], value: float) -> float:
    """value 在 history 里的分位（0~1），history 无序亦可。"""
    if not history:
        return 0.5
    return sum(1 for x in history if x < value) / len(history)


def _rank(values: list[float]) -> list[float]:
    """平均秩（并列取平均），供 Spearman 用。"""
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
    """Spearman 秩相关（IC 的定义）。输入等长。"""
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


def hit_rate(x: list[float], y: list[float]) -> float:
    """方向命中率：符号一致的比例（0 值算命中）。"""
    n = len(x)
    if n == 0:
        return 0.0
    return sum(1 for a, b in zip(x, y) if (a > 0) == (b > 0) or a == 0 or b == 0) / n


# ============================================================ 评估：OHLCV 因子
#: 参与评估的因子（价格/成交量能算出来的；资金费率与 OI 无历史序列，不在此列）
_OHLCV_FACTORS = {
    "ema_gap_pct": "EMA 结构(趋势)",
    "rsi": "RSI",
    "ret_5": "5 根收益(动量)",
    "vol_ratio": "量比",
    "adx": "ADX(趋势强度)",
}


def evaluate_factors(symbol: str, interval: str, bars: int, forward: int) -> dict[str, Any]:
    """OHLCV 五因子对「未来 N 根收益」的 IC。"""
    from ..engine.strategy import build_features
    from ..services.settings import StrategySettings

    candles = load_klines(symbol, interval, bars)
    if len(candles) < 120:
        return {"error": f"{symbol} 数据不足 {len(candles)} 根"}
    # interval 已在调用侧按 timeframe 白名单校验过；这里直接构造避免多一层字符串收窄
    cfg = StrategySettings(timeframe=interval)  # type: ignore[arg-type]
    series: dict[str, list[float]] = {k: [] for k in _OHLCV_FACTORS}
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
        for key in _OHLCV_FACTORS:
            series[key].append(float(f.get(key) or 0.0))

    rows: list[dict[str, Any]] = []
    for key, label in _OHLCV_FACTORS.items():
        ic = spearman(series[key], fwd)
        rows.append(
            {
                "factor": key,
                "label": label,
                "ic": round(ic, 4),
                "hit_rate": round(hit_rate(series[key], fwd) * 100, 1),
                "n": len(fwd),
            }
        )
    rows.sort(key=lambda r: -abs(r["ic"]))
    return {
        "symbol": symbol,
        "interval": interval,
        "forward_bars": forward,
        "samples": len(fwd),
        "factors": rows,
    }


# ============================================================ 评估：资金费率分位因子
def evaluate_funding(
    symbol: str,
    *,
    interval: str = "4h",
    bars: int = 2000,
    lookback: int = 90,
    forwards: tuple[int, ...] = (1, 3, 6),
) -> dict[str, Any]:
    """资金费率分位因子对「未来 N 根收益」的 IC。

    资金费率极端（多头/空头拥挤）是公认的反向信号，理论上 IC 应为负。
    """
    candles = load_klines(symbol, interval, bars)
    funding = load_funding(symbol, max(1000, lookback + bars // 2))
    f_times = [t for t, _ in funding]
    f_rates = [r for _, r in funding]

    series: dict[str, list[float]] = {"funding_raw": [], "funding_pct": []}
    fwd: dict[int, list[float]] = {k: [] for k in forwards}

    warmup = max(lookback, 1)
    for i in range(1, len(candles) - max(forwards)):
        ts = int(candles[i].open_time)
        # 该时刻之前最近一次已结算的资金费率（避免未来函数）
        j = bisect.bisect_right(f_times, ts) - 1
        if j < warmup:
            continue
        cur = f_rates[j]
        hist = f_rates[max(0, j - lookback) : j]
        pct = percentile(hist, cur)
        series["funding_raw"].append(cur)
        series["funding_pct"].append(pct)
        close = candles[i].close
        for fwd_bars in forwards:
            fwd[fwd_bars].append((candles[i + fwd_bars].close - close) / close * 100)

    rows: list[dict[str, Any]] = []
    for name in ("funding_raw", "funding_pct"):
        for fwd_bars in forwards:
            ic = spearman(series[name], fwd[fwd_bars])
            rows.append(
                {
                    "factor": name,
                    "forward_bars": fwd_bars,
                    "ic": round(ic, 4),
                    "hit_rate": round(hit_rate(series[name], fwd[fwd_bars]) * 100, 1),
                    "n": len(fwd[fwd_bars]),
                }
            )
    return {
        "symbol": symbol,
        "interval": interval,
        "lookback": lookback,
        "samples": len(fwd[forwards[0]]),
        "rows": rows,
    }


# ============================================================ 评估：CVD 订单流因子
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
    接口限速较严（约 60 req/min），历史数据量大，拉取本身就可能很慢。
    """
    import httpx

    candles = load_klines(symbol, interval, bars)
    earliest = int(candles[0].open_time)
    taker_buy: dict[int, float] = {}
    taker_sell: dict[int, float] = {}
    end_ms: int | None = None
    per = 1000
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
        batch = resp.json()
        if not isinstance(batch, list) or not batch:
            break
        for r in batch:
            t = int(r["T"])
            bucket = t - (t % (4 * 3600 * 1000))  # 按 4h 分桶
            qty = float(r["q"])
            if r["m"]:
                taker_buy[bucket] = taker_buy.get(bucket, 0.0) + qty
            else:
                taker_sell[bucket] = taker_sell.get(bucket, 0.0) + qty
        end_ms = int(batch[0]["T"]) - 1
        if len(batch) < per:
            break
        time.sleep(0.25)

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

    rows: list[dict[str, Any]] = []
    for fwd_bars in forwards:
        ic = spearman(series, fwd[fwd_bars])
        rows.append(
            {
                "factor": "cvd",
                "forward_bars": fwd_bars,
                "ic": round(ic, 4),
                "hit_rate": round(hit_rate(series, fwd[fwd_bars]) * 100, 1),
                "n": len(fwd[fwd_bars]),
            }
        )
    return {
        "symbol": symbol,
        "interval": interval,
        "lookback": lookback,
        "samples": len(series),
        "rows": rows,
    }


# ============================================================ 评估：信号源事后命中率
def evaluate_signal_sources(interval: str = "15m", forward: int = 6) -> dict[str, Any]:
    """按频道聚合 open 信号的事后命中率（读 signal_follows 表）。"""
    from ..db import session_scope
    from ..models import SignalFollow

    signals: list[dict[str, Any]] = []
    with session_scope() as s:
        rows = s.query(SignalFollow).order_by(SignalFollow.created_at).all()
        for r in rows:
            payload = r.payload or {}
            ts = payload.get("signal_ts") or r.created_at
            entry = payload.get("entry") or r.price or 0.0
            if r.side in ("long", "short") and ts:
                signals.append(
                    {
                        "channel": r.channel or "(未标注)",
                        "symbol": r.symbol,
                        "side": r.side,
                        "ts": int(ts),
                        "entry": float(entry),
                        "status": r.status,
                    }
                )

    if not signals:
        return {"error": "signal_follows 里没有可评估的 open 信号（先接入信号源跑 dry_run）"}

    bar_ms = {
        "1m": 60_000, "3m": 180_000, "5m": 300_000, "15m": 900_000,
        "30m": 1_800_000, "1h": 3_600_000, "4h": 14_400_000,
    }[interval]
    cache: dict[str, list[Any]] = {}

    def future_return(sym: str, ts: int, entry: float) -> float | None:
        if sym not in cache:
            # 从「信号时刻 + 前瞻窗口」往前拉，覆盖到信号点附近
            cache[sym] = load_klines(sym, interval, 300, end_ms=ts + bar_ms * (forward + 12))
        klines = cache[sym]
        times = [int(k.open_time) for k in klines]
        i = bisect.bisect_left(times, ts)
        if i + forward >= len(klines):
            return None
        base = entry if entry > 0 else klines[i].close
        return (klines[i + forward].close - base) / base * 100

    per_channel: dict[str, dict[str, Any]] = defaultdict(
        lambda: {"n": 0, "hits": 0, "ret": 0.0, "symbols": set()}
    )
    for sig in signals:
        if sig["status"] not in ("opened", "dry_run", "processing"):
            continue
        ret = future_return(sig["symbol"], sig["ts"], sig["entry"])
        if ret is None:
            continue
        d = per_channel[sig["channel"]]
        d["n"] += 1
        d["ret"] += ret
        d["symbols"].add(sig["symbol"])
        ok = (sig["side"] == "long" and ret > 0) or (sig["side"] == "short" and ret < 0)
        if ok:
            d["hits"] += 1

    return {
        "interval": interval,
        "forward_bars": forward,
        "channels": [
            {
                "channel": ch,
                "signals": d["n"],
                "hit_rate": round(d["hits"] / d["n"] * 100, 1) if d["n"] else 0.0,
                "avg_return_pct": round(d["ret"] / d["n"], 3) if d["n"] else 0.0,
                "symbols": sorted(d["symbols"]),
            }
            for ch, d in sorted(per_channel.items(), key=lambda kv: -kv[1]["n"])
        ],
    }


# ============================================================ 兼容别名
#: 旧 ``factor_ic.evaluate`` 的等价入口
evaluate = evaluate_factors


# ============================================================ CLI
def _split_symbols(raw: str) -> list[str]:
    return [s.strip() for s in raw.split(",") if s.strip()]


def _verdict_abs(ic: float) -> str:
    """换信息源类因子的判定（按 |IC| 分档）。"""
    if abs(ic) >= 0.10:
        return "强预测力"
    if abs(ic) >= 0.05:
        return "中等"
    return "弱"


def _print_advanced(rows_by_symbol: list[tuple[str, dict[str, Any]]]) -> None:
    print(f"{'标的':10} {'因子':14} {'前瞻':>4} {'IC':>8} {'命中率':>8} {'样本':>6}  判定")
    print("-" * 78)
    for sym, r in rows_by_symbol:
        for row in r["rows"]:
            print(
                f"{sym:10} {row['factor']:14} {row['forward_bars']:>4} "
                f"{row['ic']:>8.4f} {row['hit_rate']:>7.1f}% {row['n']:>6}  {_verdict_abs(row['ic'])}"
            )


def main_factors(argv: list[str] | None = None) -> int:
    """旧 ``factor_ic`` 的 CLI（保持参数与输出不变）。"""
    import argparse

    parser = argparse.ArgumentParser(description="因子 IC 评估")
    parser.add_argument("--symbols", default="BTCUSDT,ETHUSDT,SOLUSDT")
    parser.add_argument("--interval", default="15m")
    parser.add_argument("--bars", type=int, default=3000, help="K 线根数（分页取，可 >1500）")
    parser.add_argument("--forward", type=int, default=3, help="预测未来 N 根")
    args = parser.parse_args(argv)

    print(f"{'因子':14} {'说明':18} {'IC':>8} {'命中率':>8} {'样本':>7}  判定")
    print("-" * 78)
    for raw in _split_symbols(args.symbols):
        try:
            result = evaluate_factors(raw, args.interval, args.bars, args.forward)
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


def main_advanced(argv: list[str] | None = None) -> int:
    """旧 ``factor_ic_advanced`` 的 CLI。"""
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

    symbols = _split_symbols(args.symbols)

    if args.funding:
        buf: list[tuple[str, dict[str, Any]]] = []
        for sym in symbols:
            try:
                buf.append((sym, evaluate_funding(sym, interval=args.interval, bars=args.bars)))
            except Exception as exc:  # noqa: BLE001
                print(f"{sym:10} 失败 {exc}")
        if buf:
            _print_advanced(buf)

    if args.cvd:
        print("")
        buf = []
        for sym in symbols:
            try:
                buf.append((sym, evaluate_cvd(sym, interval=args.interval, bars=min(args.bars, 500))))
            except Exception as exc:  # noqa: BLE001
                print(f"{sym:10} 失败 {exc}")
        if buf:
            _print_advanced(buf)
    return 0


def main_signals(argv: list[str] | None = None) -> int:
    """旧 ``signal_source_eval`` 的 CLI。"""
    import argparse

    parser = argparse.ArgumentParser(description="信号源质量评估（事后命中率）")
    parser.add_argument("--interval", default="15m")
    parser.add_argument("--forward", type=int, default=6, help="预测未来 N 根")
    args = parser.parse_args(argv)

    r = evaluate_signal_sources(args.interval, args.forward)
    if r.get("error"):
        print(r["error"])
        return 1
    print(f"{'频道':20} {'信号':>5} {'命中率':>8} {'平均收益%':>10}  标的")
    print("-" * 72)
    for c in r["channels"]:
        verdict = "值得跟" if c["hit_rate"] >= 55 else ("存疑" if c["hit_rate"] >= 52 else "无信息量")
        print(
            f"{c['channel']:20} {c['signals']:>5} {c['hit_rate']:>7.1f}% "
            f"{c['avg_return_pct']:>9.2f}%  {','.join(c['symbols']):10}  {verdict}"
        )
    return 0


def main(argv: list[str] | None = None) -> int:
    """统一 CLI：--mode 选择评估内容。

    ``--mode all`` 依次跑 OHLCV 因子 + 资金费率（CVD 因接口限速默认不在 all 里）。
    """
    import argparse

    parser = argparse.ArgumentParser(description="因子实验室：因子 IC / 信号源命中率")
    parser.add_argument(
        "--mode", default="all",
        choices=["all", "factors", "funding", "cvd", "signals"],
        help="评估内容；默认 all（factors+funding）",
    )
    parser.add_argument("--symbols", default="BTCUSDT,ETHUSDT,SOLUSDT")
    parser.add_argument("--interval", default=None, help="留空按 mode 取默认（factors=15m, 其余=4h）")
    parser.add_argument("--bars", type=int, default=None, help="留空按 mode 取默认")
    parser.add_argument("--forward", type=int, default=3, help="预测未来 N 根（factors/signals）")
    args = parser.parse_args(argv)

    symbols = _split_symbols(args.symbols)

    if args.mode in ("all", "factors"):
        interval = args.interval or "15m"
        bars = args.bars or 3000
        print(f"### OHLCV 因子 IC（{interval}, {bars} 根）")
        main_factors(
            ["--symbols", args.symbols, "--interval", interval,
             "--bars", str(bars), "--forward", str(args.forward)]
        )

    if args.mode in ("all", "funding"):
        interval = args.interval or "4h"
        bars = args.bars or 2000
        print(f"\n### 资金费率分位因子 IC（{interval}, {bars} 根）")
        main_advanced(["--symbols", args.symbols, "--interval", interval,
                       "--bars", str(bars), "--funding"])

    if args.mode == "cvd":
        interval = args.interval or "4h"
        bars = args.bars or 500
        print(f"### CVD 订单流因子 IC（{interval}, {bars} 根）")
        main_advanced(["--symbols", args.symbols, "--interval", interval,
                       "--bars", str(bars), "--cvd"])

    if args.mode == "signals":
        print("### 信号源事后命中率")
        main_signals(["--interval", args.interval or "15m", "--forward", str(args.forward)])
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
