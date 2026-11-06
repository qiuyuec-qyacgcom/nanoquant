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

    return {
        "folds": len(items),
        "requested_folds": requested_folds,
        "train_bars": train_bars,
        "elapsed_ms": round((time.perf_counter() - started) * 1000, 1),
        "items": items,
        "aggregate": {
            "oos_return_pct": round((equity / initial_equity - 1) * 100, 3),
            "oos_avg_return_pct": round(sum(oos_returns) / len(oos_returns), 3),
            "oos_worst_fold_pct": round(min(oos_returns), 3),
            "positive_folds": positive,
            "consistency": round(positive / len(items) * 100, 1),
            "in_sample_avg_pct": round(sum(is_returns) / len(is_returns), 3),
            "overfit_gap_pct": round(sum(is_returns) / len(is_returns) - sum(oos_returns) / len(oos_returns), 3),
            "oos_trades": trade_count,
        },
        "disclaimer": "每折参数在训练段选出、测试段冻结; 仍不含 AI 复核与点差闸门, 结论只用于判断参数稳健性",
    }
