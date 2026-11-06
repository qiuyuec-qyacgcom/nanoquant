"""参数网格扫描: 在历史 K 线上粗筛参数组合.

只做**排序**, 不做最优声明:
- 单次回测约 30ms (1000 根 5m), 因此组合上限 48 个, 总耗时约 1.5s;
- 结果是「在样本内表现最好的几组」, 过拟合风险由使用者自己承担;
- 仍然不含 AI 复核与盘口点差闸门.
"""
from __future__ import annotations

import time
from itertools import product
from typing import Any

from ..services.settings import StrategySettings
from .engine import run_backtest

#: 可扫描的参数 (其余参数沿用当前配置)
GRID_KEYS = ("entry_threshold", "sl_atr_mult", "tp_atr_mult", "trailing_pct")
MAX_RUNS = 48


def run_grid(
    candles: list[Any],
    base_cfg: StrategySettings,
    *,
    symbol: str = "",
    grid: dict[str, list[float]],
    fee_bps: float = 5.0,
    slippage_bps: float = 2.0,
    spread_bps: float = 0.0,
    funding_bps_8h: float = 1.0,
    initial_equity: float = 100.0,
    risk_per_trade_pct: float = 1.0,
    margin_pct: float = 50.0,
    leverage: float = 20.0,
) -> dict[str, Any]:
    keys = [key for key in GRID_KEYS if grid.get(key)]
    values = [grid[key] for key in keys]
    combos = list(product(*values)) if keys else [()]
    truncated = len(combos) > MAX_RUNS
    combos = combos[:MAX_RUNS]

    started = time.perf_counter()
    items: list[dict[str, Any]] = []
    for combo in combos:
        params = dict(zip(keys, combo))
        cfg = base_cfg.model_copy(update=params) if params else base_cfg
        result = run_backtest(
            candles,
            cfg,
            symbol=symbol,
            fee_bps=fee_bps,
            slippage_bps=slippage_bps,
            spread_bps=spread_bps,
            funding_bps_8h=funding_bps_8h,
            initial_equity=initial_equity,
            risk_per_trade_pct=risk_per_trade_pct,
            margin_pct=margin_pct,
            leverage=leverage,
        )
        items.append(
            {
                "params": {key: round(float(value), 4) for key, value in params.items()},
                "summary": {
                    "trades": result.summary.get("trades", 0),
                    "win_rate": result.summary.get("win_rate", 0.0),
                    "profit_factor": result.summary.get("profit_factor", 0.0),
                    "expectancy": result.summary.get("expectancy", 0.0),
                    "total_return_pct": result.summary.get("total_return_pct", 0.0),
                    "max_drawdown_pct": result.summary.get("max_drawdown_pct", 0.0),
                    "avg_bars": result.summary.get("avg_bars", 0.0),
                },
            }
        )

    items.sort(
        key=lambda row: (row["summary"]["total_return_pct"], row["summary"]["profit_factor"]),
        reverse=True,
    )
    return {
        "runs": len(items),
        "truncated": truncated,
        "max_runs": MAX_RUNS,
        "elapsed_ms": round((time.perf_counter() - started) * 1000, 1),
        "grid": {key: grid[key] for key in keys},
        "best": items[0] if items else None,
        "items": items,
        "disclaimer": "样本内排序, 过拟合风险自负; 不含 AI 复核与盘口点差闸门",
    }
