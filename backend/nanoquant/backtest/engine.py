"""轻量向量化回测 (只回测纯量化部分).

边界必须说清楚:
- **不含** AI 复核 (无法复现当时的模型输出);
- **不含** 盘口点差闸门 (历史 K 线没有买卖价);
- 手续费与滑点按显式参数计入, 默认 taker 5bps + 滑点 2bps.

用途只有一个: **参数粗筛**与"逻辑有没有写错"的回归验证,
不能把这里的收益当成实盘预期。
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from ..engine.strategy import build_features, score_signal
from ..services.settings import StrategySettings

WARMUP = 60


@dataclass
class BacktestTrade:
    side: str
    entry_ts: int
    exit_ts: int
    entry: float
    exit: float
    qty: float
    pnl: float
    pnl_pct: float
    bars: int
    reason: str
    score: float
    sl_pct: float = 0.0
    tp_pct: float = 0.0
    entry_fee: float = 0.0


@dataclass
class BacktestResult:
    trades: list[BacktestTrade] = field(default_factory=list)
    equity_curve: list[float] = field(default_factory=list)
    summary: dict[str, Any] = field(default_factory=dict)


def _fill(price: float, bps: float, side: int, opening: bool) -> float:
    """把滑点按不利方向打进成交价. side: 1=long, -1=short.

    - 多头开仓买贵、平仓卖便宜;
    - 空头开仓卖便宜、平仓买贵.
    """
    rate = bps / 10000.0
    sign = 1 if (opening and side == 1) or (not opening and side == -1) else -1
    return price * (1 + sign * rate)


def _roi_target(table: list[list[float]], minutes: float) -> float | None:
    target: float | None = None
    for row in table or []:
        try:
            at, pct = float(row[0]), float(row[1])
        except (TypeError, ValueError, IndexError):
            continue
        if minutes >= at:
            target = pct
        else:
            break
    return target


def _bar_minutes(candles: list[Any]) -> float:
    if len(candles) < 2:
        return 1.0
    delta = int(getattr(candles[-1], "open_time", 0)) - int(getattr(candles[-2], "open_time", 0))
    return max(1.0, delta / 60000.0) if delta else 1.0


def run_backtest(
    candles: list[Any],
    cfg: StrategySettings,
    *,
    symbol: str = "",
    fee_bps: float = 5.0,
    slippage_bps: float = 2.0,
    initial_equity: float = 100.0,
    risk_per_trade_pct: float = 1.0,
    margin_pct: float = 50.0,
    leverage: float = 20.0,
    max_bars_hold: int | None = None,
) -> BacktestResult:
    result = BacktestResult()
    if len(candles) <= WARMUP:
        result.summary = {"error": f"K 线不足 {WARMUP + 1} 根"}
        return result

    fee = fee_bps / 10000.0
    bar_minutes = _bar_minutes(candles)
    # 24h 成交额近似: 用滚动窗口的 quote volume 之和 (历史 K 线没有独立的 24h 快照),
    # 否则 score_signal 的「24h 成交额偏低」闸门会把所有信号都拦掉.
    window_bars = max(1, int(24 * 60 / bar_minutes))
    quote = [float(c.volume) * float(c.close) for c in candles]
    cum = [0.0]
    for value in quote:
        cum.append(cum[-1] + value)
    equity = initial_equity
    peak_equity = equity
    max_dd = 0.0
    fee_total = 0.0
    hold_limit = max_bars_hold or cfg.max_hold_minutes

    pos: BacktestTrade | None = None
    entry_idx = 0
    trail_peak = 0.0

    def close_pos(exit_price: float, idx: int, reason: str) -> None:
        nonlocal equity, peak_equity, max_dd, fee_total, pos
        if pos is None:
            return
        direction = 1 if pos.side == "long" else -1
        exit_fill = _fill(exit_price, slippage_bps, direction, opening=False)
        gross = (exit_fill - pos.entry) * pos.qty * direction
        exit_fee = exit_fill * pos.qty * fee
        pnl = gross - exit_fee - pos.entry_fee
        fee_total += exit_fee
        equity += pnl
        peak_equity = max(peak_equity, equity)
        dd = (peak_equity - equity) / peak_equity * 100 if peak_equity > 0 else 0.0
        max_dd = max(max_dd, dd)
        pos.exit = round(exit_fill, 8)
        pos.exit_ts = int(getattr(candles[idx], "open_time", 0))
        pos.pnl = round(pnl, 6)
        pos.pnl_pct = round((exit_fill / pos.entry - 1) * 100 * direction, 4)
        pos.bars = idx - entry_idx
        pos.reason = reason
        result.trades.append(pos)
        result.equity_curve.append(round(equity, 6))
        pos = None

    for i in range(WARMUP, len(candles) - 1):
        bar = candles[i]

        # ---------------- 持仓管理 ----------------
        if pos is not None:
            direction = 1 if pos.side == "long" else -1
            held_bars = i - entry_idx

            if pos.sl_pct > 0:
                sl_price = pos.entry * (1 - pos.sl_pct / 100) if direction == 1 else pos.entry * (1 + pos.sl_pct / 100)
                if (direction == 1 and bar.low <= sl_price) or (direction == -1 and bar.high >= sl_price):
                    close_pos(sl_price, i, "stop_loss")
                    continue

            if pos.tp_pct > 0:
                tp_price = pos.entry * (1 + pos.tp_pct / 100) if direction == 1 else pos.entry * (1 - pos.tp_pct / 100)
                if (direction == 1 and bar.high >= tp_price) or (direction == -1 and bar.low <= tp_price):
                    close_pos(tp_price, i, "take_profit")
                    continue

            if cfg.trailing_pct > 0:
                if direction == 1:
                    trail_peak = max(trail_peak, bar.high)
                    if bar.close <= trail_peak * (1 - cfg.trailing_pct / 100):
                        close_pos(bar.close, i, "trailing_stop")
                        continue
                else:
                    trail_peak = min(trail_peak if trail_peak else bar.low, bar.low)
                    if bar.close >= trail_peak * (1 + cfg.trailing_pct / 100):
                        close_pos(bar.close, i, "trailing_stop")
                        continue

            if cfg.roi_table:
                target = _roi_target(cfg.roi_table, held_bars * bar_minutes)
                if target is not None and target > 0:
                    profit_pct = (bar.close - pos.entry) * direction / pos.entry * 100
                    if profit_pct >= target:
                        close_pos(bar.close, i, "roi")
                        continue

            if cfg.max_hold_minutes > 0 and held_bars * bar_minutes >= min(hold_limit, cfg.max_hold_minutes):
                close_pos(bar.close, i, "timeout")
                continue
            continue

        # ---------------- 开仓判断 ----------------
        window = candles[max(0, i - WARMUP + 1) : i + 1]
        qv_24h = cum[i + 1] - cum[max(0, i + 1 - window_bars)]
        features = build_features(window, cfg, quote_volume=qv_24h)
        sig = score_signal(symbol, features, cfg)
        if sig.direction == "flat" or abs(sig.score) < cfg.entry_threshold:
            continue

        direction = 1 if sig.direction == "long" else -1
        next_bar = candles[i + 1]
        entry = _fill(next_bar.open, slippage_bps, direction, opening=True)
        sl_pct = sig.sl_pct or 0.0
        if sl_pct <= 0 or entry <= 0:
            continue

        risk_amount = equity * risk_per_trade_pct / 100
        qty = risk_amount / (entry * sl_pct / 100)
        max_notional = equity * (margin_pct / 100) * leverage
        if qty * entry > max_notional:
            qty = max_notional / entry
        if qty <= 0:
            continue

        pos = BacktestTrade(
            side=sig.direction,
            entry_ts=int(getattr(next_bar, "open_time", 0)),
            exit_ts=0,
            entry=round(entry, 8),
            exit=0.0,
            qty=qty,
            pnl=0.0,
            pnl_pct=0.0,
            bars=0,
            reason="",
            score=sig.score,
            sl_pct=sl_pct,
            tp_pct=sig.tp_pct or 0.0,
            entry_fee=entry * qty * fee,
        )
        fee_total += pos.entry_fee
        entry_idx = i + 1
        trail_peak = next_bar.open

    # 未平仓的按最后一根收盘强制平掉, 避免收益虚高
    if pos is not None:
        close_pos(candles[-1].close, len(candles) - 1, "timeout")

    result.summary = _summarize(result, equity, initial_equity, fee_total, max_dd, bar_minutes)
    return result


def _bucket_stats(trades: list[BacktestTrade]) -> dict[str, Any]:
    if not trades:
        return {"trades": 0, "wins": 0, "win_rate": 0.0, "pnl": 0.0}
    wins = sum(1 for t in trades if t.pnl > 0)
    return {
        "trades": len(trades),
        "wins": wins,
        "win_rate": round(wins / len(trades) * 100, 2),
        "pnl": round(sum(t.pnl for t in trades), 6),
    }


def _segments(trades: list[BacktestTrade], bar_minutes: float) -> dict[str, Any]:
    """按开仓小时 / 星期 / 持仓时长 / 出场原因分组, 用来看策略的时间偏好."""
    from datetime import datetime, timezone

    hours: dict[int, list[BacktestTrade]] = {}
    weekdays: dict[int, list[BacktestTrade]] = {}
    holds: dict[str, list[BacktestTrade]] = {}
    exits: dict[str, int] = {}

    for trade in trades:
        moment = datetime.fromtimestamp(int(trade.entry_ts) / 1000, tz=timezone.utc)
        hours.setdefault(moment.hour, []).append(trade)
        weekdays.setdefault(moment.weekday(), []).append(trade)

        minutes = max(0.0, trade.bars * bar_minutes)
        if minutes < 15:
            key = "<15m"
        elif minutes < 60:
            key = "15-60m"
        elif minutes < 180:
            key = "60-180m"
        else:
            key = ">=180m"
        holds.setdefault(key, []).append(trade)
        exits[trade.reason] = exits.get(trade.reason, 0) + 1

    weekday_names = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]
    return {
        "by_hour": [
            {"hour": hour, **_bucket_stats(items)} for hour, items in sorted(hours.items())
        ],
        "by_weekday": [
            {"weekday": weekday_names[day], **_bucket_stats(items)}
            for day, items in sorted(weekdays.items())
        ],
        "by_hold": [
            {"bucket": key, **_bucket_stats(holds[key])}
            for key in ("<15m", "15-60m", "60-180m", ">=180m")
            if key in holds
        ],
        "by_exit": [{"reason": reason, "count": count} for reason, count in sorted(exits.items(), key=lambda kv: -kv[1])],
    }


def _summarize(
    result: BacktestResult,
    equity: float,
    initial: float,
    fee_total: float,
    max_dd: float,
    bar_minutes: float = 5.0,
) -> dict[str, Any]:
    trades = result.trades
    wins = [t for t in trades if t.pnl > 0]
    losses = [t for t in trades if t.pnl <= 0]
    gross_win = sum(t.pnl for t in wins)
    gross_loss = abs(sum(t.pnl for t in losses))
    return {
        "trades": len(trades),
        "wins": len(wins),
        "losses": len(losses),
        "win_rate": round(len(wins) / len(trades) * 100, 2) if trades else 0.0,
        "profit_factor": round(gross_win / gross_loss, 3)
        if gross_loss > 0
        else (round(gross_win, 3) if gross_win else 0.0),
        "avg_win": round(gross_win / len(wins), 6) if wins else 0.0,
        "avg_loss": round(-gross_loss / len(losses), 6) if losses else 0.0,
        "expectancy": round(sum(t.pnl for t in trades) / len(trades), 6) if trades else 0.0,
        "avg_bars": round(sum(t.bars for t in trades) / len(trades), 1) if trades else 0.0,
        "total_return_pct": round((equity / initial - 1) * 100, 3),
        "max_drawdown_pct": round(max_dd, 3),
        "fee_total": round(fee_total, 6),
        "final_equity": round(equity, 6),
        "disclaimer": "不含 AI 复核与盘口点差闸门, 仅用于参数粗筛, 不代表实盘收益",
        **_segments(trades, bar_minutes),
    }
