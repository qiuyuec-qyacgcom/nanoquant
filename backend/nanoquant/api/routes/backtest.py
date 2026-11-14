"""回测接口: 拉一段历史 K 线, 用与实盘相同的 score_signal 跑一遍."""
from __future__ import annotations

import asyncio

from fastapi import APIRouter, Depends, HTTPException

from ...backtest.engine import run_backtest
from ...backtest.grid import run_grid
from ...backtest.walkforward import run_walkforward
from ...exchange.client import BinanceError, NoCredentials
from ...exchange.client import client as binance_client
from ...services.settings import get_risk, get_strategy
from ...services.symbols import ordered_symbols
from ..deps import current_user
from ..schemas import BacktestGridIn, BacktestIn, WalkforwardIn

router = APIRouter(tags=["backtest"])

ALLOWED = {"5m", "15m", "30m", "1h", "4h", "1d"}

#: 网格参数的合法区间 (与 StrategySettings 保持一致)
GRID_BOUNDS = {
    "entry_threshold": (0.05, 0.95),
    "sl_atr_mult": (0.2, 10.0),
    "tp_atr_mult": (0.2, 20.0),
    "trailing_pct": (0.0, 20.0),
}


def _guard(payload: BacktestIn) -> str:
    symbol = payload.symbol.upper()
    if payload.interval not in ALLOWED:
        raise HTTPException(status_code=400, detail=f"周期只支持 {sorted(ALLOWED)}")
    if symbol not in ordered_symbols(enabled_only=False):
        raise HTTPException(status_code=404, detail=f"{symbol} 不在标的池中")
    return symbol


async def _candles(symbol: str, payload: BacktestIn) -> list:
    try:
        candles = await binance_client.klines(symbol, payload.interval, payload.limit, fresh=False)
    except (BinanceError, NoCredentials) as exc:
        raise HTTPException(status_code=503, detail=f"历史 K 线不可用: {exc}") from exc
    if len(candles) < 100:
        raise HTTPException(status_code=400, detail=f"K 线不足 (仅 {len(candles)} 根)")
    return candles


def _fee_bps(payload: BacktestIn) -> float:
    """回测手续费：请求里显式给了就用，否则跟实盘风控配置保持一致.

    risk.fee_rate_taker 存的是比例（0.0005 = 5bps）。以前 API 默认值写死 5，
    前端虽然会同步，但脚本/直接调接口的人改了风控费率后回测仍按旧值算。
    """
    if payload.fee_bps is not None:
        return float(payload.fee_bps)
    return round(float(get_risk().fee_rate_taker) * 10_000, 4)


def _costs(payload: BacktestIn) -> dict:
    risk = get_risk()
    return {
        "fee_bps": _fee_bps(payload),
        "slippage_bps": payload.slippage_bps,
        "spread_bps": payload.spread_bps,
        "funding_bps_8h": payload.funding_bps_8h,
        "initial_equity": payload.initial_equity,
        "risk_per_trade_pct": risk.risk_per_trade_pct,
        "margin_pct": risk.margin_pct,
        "leverage": float(risk.default_leverage),
    }


def _grid_payload(payload: BacktestGridIn) -> dict[str, list[float]]:
    grid: dict[str, list[float]] = {}
    for key, values in (
        ("entry_threshold", payload.entry_threshold),
        ("sl_atr_mult", payload.sl_atr_mult),
        ("tp_atr_mult", payload.tp_atr_mult),
        ("trailing_pct", payload.trailing_pct),
    ):
        if not values:
            continue
        low, high = GRID_BOUNDS[key]
        for value in values:
            if not low <= value <= high:
                raise HTTPException(status_code=400, detail=f"{key}={value} 超出允许范围 [{low}, {high}]")
        grid[key] = [float(v) for v in values]
    if not grid:
        raise HTTPException(status_code=400, detail="至少需要给一个参数一组取值")
    return grid


def _meta(payload: BacktestIn, symbol: str, candles: list) -> dict:
    return {
        "symbol": symbol,
        "interval": payload.interval,
        "bars": len(candles),
        "from_ts": int(getattr(candles[0], "open_time", 0)),
        "to_ts": int(getattr(candles[-1], "open_time", 0)),
        "assumptions": {
            "fee_bps": _fee_bps(payload),
            "slippage_bps": payload.slippage_bps,
            "spread_bps": payload.spread_bps,
            "funding_bps_8h": payload.funding_bps_8h,
        },
    }


@router.post("/backtest/run")
async def run(payload: BacktestIn, user: str = Depends(current_user)) -> dict:
    """跑一次回测 (纯量化部分)."""
    symbol = _guard(payload)
    candles = await _candles(symbol, payload)
    cfg = get_strategy()

    result = run_backtest(candles, cfg, symbol=symbol, **_costs(payload))

    # summary 里的 trades 是"笔数", 而下面的 trades 是明细数组:
    # 直接展开会被覆盖, 导致前端把数组当成笔数渲染, 因此改名成 trade_count
    body = dict(result.summary)
    body["trade_count"] = body.pop("trades", 0)

    return {
        **_meta(payload, symbol, candles),
        **body,
        "equity_curve": result.equity_curve[-300:],
        "trades": [
            {
                "side": t.side,
                "entry_ts": t.entry_ts,
                "exit_ts": t.exit_ts,
                "entry": t.entry,
                "exit": t.exit,
                "qty": round(t.qty, 8),
                "pnl": t.pnl,
                "pnl_pct": t.pnl_pct,
                "bars": t.bars,
                "reason": t.reason,
                "score": t.score,
                "funding": t.funding,
                "partial_tp": t.partial_tp,
            }
            for t in result.trades[-100:]
        ],
    }


@router.post("/backtest/grid")
async def run_grid_endpoint(payload: BacktestGridIn, user: str = Depends(current_user)) -> dict:
    """参数网格扫描: 一次跑多组参数, 按样本内收益排序 (粗筛, 非最优声明)."""
    symbol = _guard(payload)
    grid = _grid_payload(payload)
    candles = await _candles(symbol, payload)

    result = await asyncio.to_thread(
        run_grid,
        candles,
        get_strategy(),
        symbol=symbol,
        grid=grid,
        **_costs(payload),
    )
    return {**_meta(payload, symbol, candles), **result}


@router.post("/backtest/walkforward")
async def run_walkforward_endpoint(payload: WalkforwardIn, user: str = Depends(current_user)) -> dict:
    """滚动样本外验证: 每折用前段选参数、后段冻结检验, 给出过拟合差距."""
    symbol = _guard(payload)
    grid = _grid_payload(payload)
    candles = await _candles(symbol, payload)
    if len(candles) <= 240:
        raise HTTPException(status_code=400, detail=f"K 线不足: 至少需要 241 根 (当前 {len(candles)})")

    result = await asyncio.to_thread(
        run_walkforward,
        candles,
        get_strategy(),
        symbol=symbol,
        grid=grid,
        folds=payload.folds,
        train_bars=payload.train_bars,
        **_costs(payload),
    )
    if result.get("error"):
        raise HTTPException(status_code=400, detail=result["error"])
    return {**_meta(payload, symbol, candles), **result}
