"""滚动样本外验证 (walk-forward).

动机: 参数网格只做**样本内**排序, 好参数很可能只是过拟合。
这里把历史切成若干段, 每一段都用"前一段训练、后一段检验"：

    训练窗口 ─────► 选最优参数
                         │
                         └─► 测试窗口 (参数冻结) ──► 记录 OOS 表现

汇总时给出 `overfit_gap`（样本内均值 − 样本外均值）与正收益折数占比：
gap 越大、正折越少，说明参数越可能是在拟合噪声。
"""
from __future__ import annotations

import time
from typing import Any

from ..services.settings import StrategySettings
from .engine import WARMUP, run_backtest
from .grid import run_grid

#: 折数上限: 每折都要跑一遍网格, 折数×组合数直接决定耗时
MAX_FOLDS = 6
MIN_FOLDS = 2


def _clip(value: int, low: int, high: int) -> int:
    return max(low, min(high, int(value)))


def run_walkforward(
    candles: list[Any],
    base_cfg: StrategySettings,
    *,
    symbol: str = "",
    grid: dict[str, list[float]],
    folds: int = 3,
    train_bars: int = 600,
    fee_bps: float = 5.0,
    slippage_bps: float = 2.0,
    spread_bps: float = 0.0,
    funding_bps_8h: float = 1.0,
    initial_equity: float = 100.0,
    risk_per_trade_pct: float = 1.0,
    margin_pct: float = 50.0,
    leverage: float = 20.0,
) -> dict[str, Any]:
    started = time.perf_counter()
    folds = _clip(folds, MIN_FOLDS, MAX_FOLDS)
    if len(candles) <= WARMUP * 2:
        return {"error": f"K 线不足 (至少 {WARMUP * 2 + 1} 根才能切样本内/外)"}

    requested_folds = folds
    # 训练窗口不能把历史吃光: 至少给每折留 WARMUP+1 根测试
    train_bars = max(WARMUP * 2, min(int(train_bars), len(candles) - WARMUP * 2))
    step = (len(candles) - WARMUP - train_bars) // folds
    # 数据不够就自动减折 (至少 2 折), 而不是返回一堆空的折
    while folds > MIN_FOLDS and step <= WARMUP:
        folds -= 1
        step = (len(candles) - WARMUP - train_bars) // folds
    if step <= WARMUP:
        need = WARMUP + train_bars + (WARMUP + 1)
        return {"error": f"K 线不足: 至少需要约 {need} 根 (当前 {len(candles)})"}

    costs = dict(
        fee_bps=fee_bps,
        slippage_bps=slippage_bps,
        spread_bps=spread_bps,
        funding_bps_8h=funding_bps_8h,
        initial_equity=initial_equity,
        risk_per_trade_pct=risk_per_trade_pct,
        margin_pct=margin_pct,
        leverage=leverage,
    )

    items: list[dict[str, Any]] = []
    for k in range(folds):
        # 滚动窗口: 训练段紧随测试段之前, 长度固定; 测试段只前进不回头
        train_end = WARMUP + train_bars + k * step
        test_end = min(len(candles), train_end + step)
        train_start = max(0, train_end - train_bars)
        train = candles[train_start:train_end]
        # 测试区间自带一段预热, 否则每折开头 60 根算不出特征
        test = candles[max(0, train_end - WARMUP) : test_end]
        if len(train) <= WARMUP or len(test) <= WARMUP:
            continue

        searched = run_grid(train, base_cfg, symbol=symbol, grid=grid, **costs)
        best = searched.get("best")
        if not best:
            continue

        cfg = base_cfg.model_copy(update=best["params"])
        oos = run_backtest(test, cfg, symbol=symbol, **costs)
        # 成本敏感性：手续费/滑点翻倍后，样本外结论是否翻转。
        # 短线利润薄，一倍成本往往就是"能赚"与"必亏"的分界线。
        stressed = run_backtest(
            test, cfg, symbol=symbol,
            **{**costs, "fee_bps": costs["fee_bps"] * 2, "slippage_bps": costs["slippage_bps"] * 2},
        )
        items.append(
            {
                "fold": k + 1,
                "train_from_ts": int(getattr(train[0], "open_time", 0)),
                "train_to_ts": int(getattr(train[-1], "open_time", 0)),
                "test_from_ts": int(getattr(test[0], "open_time", 0)),
                "test_to_ts": int(getattr(test[-1], "open_time", 0)),
                "train_bars": len(train),
                "test_bars": len(test),
                "params": best["params"],
                "in_sample": best["summary"],
                "out_sample": {
                    key: oos.summary.get(key, 0)
                    for key in (
                        "trades",
                        "win_rate",
                        "profit_factor",
                        "expectancy",
                        "total_return_pct",
                        "max_drawdown_pct",
                    )
                },
                "out_sample_funding": oos.summary.get("funding_total", 0.0),
                # 成本翻倍后的样本外收益（用于判断"结论是否脆弱"）
                "stressed_return_pct": stressed.summary.get("total_return_pct", 0.0),
                "stressed_expectancy": stressed.summary.get("expectancy", 0.0),
                # 逐笔收益，用于 bootstrap
                "trade_returns_pct": [t.pnl_pct for t in getattr(oos, "trades", []) or []],
            }
        )

    if not items:
        return {"error": "没有可用的折 (训练/测试窗口太短)"}

    # 样本外复合收益: 每折用自己的初始权益跑, 整体按复利拼接
    equity = initial_equity
    for item in items:
        equity *= 1 + float(item["out_sample"]["total_return_pct"]) / 100.0
    oos_returns = [float(item["out_sample"]["total_return_pct"]) for item in items]
    is_returns = [float(item["in_sample"]["total_return_pct"]) for item in items]
    trade_count = sum(int(item["out_sample"]["trades"]) for item in items)
    positive = sum(1 for value in oos_returns if value > 0)

    # bootstrap：对样本外逐笔收益重采样，估计"平均单笔收益"的分布，
    # 判断正收益是不是运气（置信区间跨 0 = 不显著）。
    all_returns = [r for item in items for r in item.get("trade_returns_pct", [])]
    boot = {"ci_low": 0.0, "ci_high": 0.0, "p_positive": 0.0, "n_trades": len(all_returns)}
    if len(all_returns) >= 10:
        import random

        rng = random.Random(42)
        means: list[float] = []
        for _ in range(500):
            sample = [rng.choice(all_returns) for _ in range(len(all_returns))]
            means.append(sum(sample) / len(sample))
        means.sort()
        boot = {
            "ci_low": round(means[int(len(means) * 0.025)], 3),
            "ci_high": round(means[int(len(means) * 0.975)], 3),
            "p_positive": round(sum(1 for m in means if m > 0) / len(means), 3),
            "n_trades": len(all_returns),
        }

    # 成本敏感性：手续费/滑点翻倍后样本外是否转负
    stressed_returns = [float(item["stressed_return_pct"]) for item in items]
    stressed_avg = sum(stressed_returns) / len(stressed_returns) if stressed_returns else 0.0
    base_avg = sum(oos_returns) / len(oos_returns) if oos_returns else 0.0

    return {
        "folds": len(items),
        "requested_folds": requested_folds,
        "train_bars": train_bars,
        "elapsed_ms": round((time.perf_counter() - started) * 1000, 1),
        "items": items,
        "aggregate": {
            "oos_return_pct": round((equity / initial_equity - 1) * 100, 3),
            "oos_avg_return_pct": round(base_avg, 3),
            "oos_worst_fold_pct": round(min(oos_returns), 3),
            "positive_folds": positive,
            "consistency": round(positive / len(items) * 100, 1),
            "in_sample_avg_pct": round(sum(is_returns) / len(is_returns), 3),
            "overfit_gap_pct": round(sum(is_returns) / len(is_returns) - base_avg, 3),
            "oos_trades": trade_count,
        },
        "bootstrap": boot,
        "cost_sensitivity": {
            "base_avg_return_pct": round(base_avg, 3),
            "stressed_avg_return_pct": round(stressed_avg, 3),
            # 成本翻倍后收益掉了多少（越负越脆弱）
            "delta_pct": round(stressed_avg - base_avg, 3),
            "flips_sign": (base_avg > 0) != (stressed_avg > 0),
        },
        "disclaimer": "每折参数在训练段选出、测试段冻结; 仍不含 AI 复核与点差闸门, 结论只用于判断参数稳健性",
    }


# ------------------------------------------------------------------ 命令行

def _load_klines(symbol: str, interval: str, limit: int) -> list[Any]:
    """分页取 K 线（突破单次 1000 根上限），不需要 API 凭证，方便离线复盘。"""
    import httpx

    from ..exchange.client import Candle

    out: list[Candle] = []
    end_ms: int | None = None
    per = 1000
    while len(out) < limit:
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
        end_ms = int(rows[0][0]) - 1
        if len(rows) < per:
            break
    return out[-limit:]


def main(argv: list[str] | None = None) -> int:
    """`python -m nanoquant.backtest.walkforward --symbols BTCUSDT --interval 30m`"""
    import argparse

    parser = argparse.ArgumentParser(description="walk-forward 验证：样本外还能不能站住")
    parser.add_argument("--symbols", default="BTCUSDT,ETHUSDT,SOLUSDT")
    parser.add_argument("--interval", default="30m")
    parser.add_argument("--limit", type=int, default=3000, help="每标的 K 线根数（分页取，可 >1000）")
    parser.add_argument("--folds", type=int, default=3)
    parser.add_argument("--train-bars", type=int, default=600)
    args = parser.parse_args(argv)

    grid = {
        "entry_threshold": [0.30, 0.45],
        "sl_atr_mult": [1.0, 1.5],
        "tp_atr_mult": [2.0, 3.0],
    }

    header = f"{'标的':10} {'折':>3} {'OOS笔':>7} {'OOS收益%':>10} {'最差折%':>9} {'正折':>7} {'过拟合缺口%':>12} {'bootstrapCI':>18} {'成本×2':>9}"
    print(header)
    print("-" * 112)
    for raw in [s.strip() for s in args.symbols.split(",") if s.strip()]:
        try:
            candles = _load_klines(raw, args.interval, args.limit)
        except Exception as exc:  # noqa: BLE001
            print(f"{raw:10} 取数失败: {str(exc)[:60]}")
            continue
        result = run_walkforward(
            candles,
            StrategySettings(timeframe=args.interval),
            symbol=raw,
            grid=grid,
            folds=args.folds,
            train_bars=args.train_bars,
        )
        if result.get("error"):
            print(f"{raw:10} {result['error']}")
            continue
        agg = result["aggregate"]
        b = result.get("bootstrap", {})
        cs = result.get("cost_sensitivity", {})
        ci = f"[{b.get('ci_low', 0)},{b.get('ci_high', 0)}]" if b else "-"
        print(
            f"{raw:10} {result['folds']:>3} {agg['oos_trades']:>7} "
            f"{agg['oos_return_pct']:>10.2f} {agg['oos_worst_fold_pct']:>9.2f} "
            f"{agg['positive_folds']}/{result['folds']:<5} {agg['overfit_gap_pct']:>12.2f} "
            f"{ci:>18} {cs.get('stressed_avg_return_pct', 0):>9.2f}"
        )
    return 0


if __name__ == "__main__":  # pragma: no cover - 手工运行入口
    raise SystemExit(main())
