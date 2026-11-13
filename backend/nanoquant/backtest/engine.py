"""轻量向量化回测 (只回测纯量化部分).

保真度边界 (必须说清楚):
- **不含** AI 复核 (无法复现当时的模型输出);
- **不含** 盘口点差**闸门** (历史 K 线没有买卖价, 无法知道当时是否会被拦下),
  但可以把点差当**成本**计入 (`spread_bps`);
- 手续费 / 滑点 / 点差按显式参数计入;
- **资金费**按假设费率 (默认 1bp / 8h) 在每个 8h 结算点从持仓上扣除,
  多付空收; 历史资金费率没有随 K 线保存, 所以这是一个假设值而非真实结算记录;
- 部分止盈与实盘同口径: R 达标先平 `partial_tp_ratio`, 剩余仓位止损上移到保本;
  同一根 K 线内若同时触及止损与部分止盈, **按止损优先**处理 (无法从 OHLC 判断先后, 取保守).

用途只有一个: **参数粗筛**与"逻辑有没有写错"的回归验证,
不能把这里的收益当成实盘预期。
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from ..engine.strategy import build_features, entry_regime, score_signal
from ..services.settings import StrategySettings

WARMUP = 60

#: 币安资金费结算周期 (00:00 / 08:00 / 16:00 UTC)
FUNDING_INTERVAL_MS = 8 * 3600 * 1000


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
    funding: float = 0.0
    partial_tp: bool = False
    #: 当前止损价 (部分止盈后上移到保本)
    stop_price: float = 0.0


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


def _gap_fill(bar: Any, trigger: float, direction: int, *, adverse: bool) -> float:
    """跳空成交价：开盘已经越过触发价时按开盘价成交，否则按触发价.

    止损/强平（``adverse=True``）跳空会成交得更差，止盈（``adverse=False``）会更好。
    旧实现一律按触发价成交，等于假设"止损永远能精确成交"——这是回测最容易高估
    收益的地方，尤其在插针行情里。
    """
    open_px = float(getattr(bar, "open", trigger) or trigger)
    if direction == 1:
        crossed = open_px <= trigger if adverse else open_px >= trigger
    else:
        crossed = open_px >= trigger if adverse else open_px <= trigger
    return open_px if crossed else trigger


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


def funding_crossings(prev_ts: int, ts: int) -> int:
    """(prev_ts, ts] 之间跨过几个 8h 结算点 (UTC 00/08/16)."""
    if ts <= prev_ts:
        return 0
    return int(ts // FUNDING_INTERVAL_MS - prev_ts // FUNDING_INTERVAL_MS)


def run_backtest(
    candles: list[Any],
    cfg: StrategySettings,
    *,
    symbol: str = "",
    fee_bps: float = 5.0,
    slippage_bps: float = 2.0,
    spread_bps: float = 0.0,
    funding_bps_8h: float = 1.0,
    initial_equity: float = 100.0,
    risk_per_trade_pct: float = 1.0,
    margin_pct: float = 50.0,
    leverage: float = 20.0,
    max_bars_hold: int | None = None,
    maintenance_margin_rate: float = 0.005,
) -> BacktestResult:
    result = BacktestResult()
    if len(candles) <= WARMUP:
        result.summary = {"error": f"K 线不足 {WARMUP + 1} 根"}
        return result

    fee = fee_bps / 10000.0
    funding_rate = funding_bps_8h / 10000.0
    # 点差按"穿价"成本计: 开平各承担一半
    slip = slippage_bps + spread_bps / 2.0
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
    funding_total = 0.0
    partial_count = 0
    liquidations = 0
    hold_limit = max_bars_hold or cfg.max_hold_minutes
    at_r = float(cfg.partial_tp_at_r or 0.0)
    ratio = float(cfg.partial_tp_ratio or 0.0)

    pos: BacktestTrade | None = None
    state: dict[str, Any] = {}
    entry_idx = 0
    trail_peak = 0.0

    def book(amount: float) -> None:
        """把一笔已实现盈亏记入权益, 并维护回撤."""
        nonlocal equity, peak_equity, max_dd
        equity += amount
        peak_equity = max(peak_equity, equity)
        dd = (peak_equity - equity) / peak_equity * 100 if peak_equity > 0 else 0.0
        max_dd = max(max_dd, dd)

    def close_pos(exit_price: float, idx: int, reason: str) -> None:
        nonlocal fee_total, pos
        if pos is None:
            return
        direction = 1 if pos.side == "long" else -1
        exit_fill = _fill(exit_price, slip, direction, opening=False)
        gross = (exit_fill - pos.entry) * pos.qty * direction
        exit_fee = exit_fill * pos.qty * fee
        entry_share = float(state["entry_fee"]) * (pos.qty / float(state["initial_qty"]))
        net = gross - exit_fee - entry_share
        fee_total += exit_fee
        book(net)

        # 明细里的 qty 用初始仓位 (pnl 已包含部分止盈腿), 否则看起来像"只交易了半仓"
        pos.qty = float(state["initial_qty"])
        pos.exit = round(exit_fill, 8)
        pos.exit_ts = int(getattr(candles[idx], "open_time", 0))
        pos.pnl = round(float(state["realized"]) + net, 6)
        pos.pnl_pct = round((exit_fill / pos.entry - 1) * 100 * direction, 4)
        pos.bars = idx - entry_idx
        pos.reason = reason
        pos.funding = round(float(state["funding"]), 6)
        pos.partial_tp = bool(state["partial"])
        result.trades.append(pos)
        result.equity_curve.append(round(equity, 6))
        pos = None

    def take_partial(price: float) -> None:
        """平掉 ratio 比例的仓位, 剩余止损移到保本 (与实盘 partial_tp 同口径)."""
        nonlocal fee_total, partial_count
        if pos is None:
            return
        direction = 1 if pos.side == "long" else -1
        leg_qty = pos.qty * ratio
        if leg_qty <= 0:
            return
        fill = _fill(price, slip, direction, opening=False)
        gross = (fill - pos.entry) * leg_qty * direction
        leg_fee = fill * leg_qty * fee
        entry_share = float(state["entry_fee"]) * (leg_qty / float(state["initial_qty"]))
        net = gross - leg_fee - entry_share
        fee_total += leg_fee
        book(net)
        state["realized"] = float(state["realized"]) + net
        state["partial"] = True
        pos.qty -= leg_qty
        # 剩余仓位止损上移到保本 (活下来比多赚更重要)
        pos.stop_price = pos.entry
        partial_count += 1

    for i in range(WARMUP, len(candles) - 1):
        bar = candles[i]

        # ---------------- 持仓管理 ----------------
        if pos is not None:
            direction = 1 if pos.side == "long" else -1
            held_bars = i - entry_idx
            prev_ts = int(getattr(candles[i - 1], "open_time", 0))

            # 1) 资金费: 跨过 8h 结算点就按当前剩余数量收取
            crossings = funding_crossings(prev_ts, int(getattr(bar, "open_time", 0)))
            if crossings > 0 and funding_rate:
                notional = float(getattr(bar, "open", pos.entry)) * pos.qty
                delta = -direction * funding_rate * notional * crossings
                book(delta)
                funding_total += delta
                state["realized"] = float(state["realized"]) + delta
                state["funding"] = float(state["funding"]) + delta

            # 2) 强平：杠杆越高越可能早于止损触发（旧实现完全没有这一步，
            #    8–30x 下会把"爆仓"记成普通止损，系统性低估风险）
            liq_price = float(state.get("liq_price") or 0.0)
            if liq_price > 0:
                hit_liq = (direction == 1 and bar.low <= liq_price) or (
                    direction == -1 and bar.high >= liq_price
                )
                if hit_liq:
                    fill = _gap_fill(bar, liq_price, direction, adverse=True)
                    close_pos(fill, i, "liquidation")
                    liquidations += 1
                    continue

            # 3) 硬止损 (最保守: 同一根 K 线里与部分止盈冲突时以止损为准)
            #    注意读 pos.stop_price: 部分止盈会把它上移到保本, 用副本会漏掉这次移动
            stop_price = float(pos.stop_price or 0.0)
            if stop_price > 0:
                hit = (direction == 1 and bar.low <= stop_price) or (direction == -1 and bar.high >= stop_price)
                if hit:
                    # 跳空穿过止损价时只能按开盘价成交；按止损价成交是回测里最经典的乐观假设
                    close_pos(_gap_fill(bar, stop_price, direction, adverse=True), i, "stop_loss")
                    continue

            # 4) 止盈（跳空高开则按开盘价成交，对空头同理）
            if pos.tp_pct > 0:
                tp_price = pos.entry * (1 + pos.tp_pct / 100) if direction == 1 else pos.entry * (1 - pos.tp_pct / 100)
                if (direction == 1 and bar.high >= tp_price) or (direction == -1 and bar.low <= tp_price):
                    close_pos(_gap_fill(bar, tp_price, direction, adverse=False), i, "take_profit")
                    continue

            # 4) 部分止盈: R 达标先落袋, 剩余仓位保本
            if at_r > 0 and ratio > 0 and not state["partial"] and pos.sl_pct > 0:
                trigger = pos.entry * (1 + direction * at_r * pos.sl_pct / 100)
                reached = (direction == 1 and bar.high >= trigger) or (direction == -1 and bar.low <= trigger)
                if reached:
                    take_partial(trigger)
                    continue

            # 5) 移动止损：与实盘同一条规则——浮盈先走到 trailing_arm_r 倍风险才 arm
            if cfg.trailing_pct > 0:
                risk_pct = pos.sl_pct or 0.0
                favor_pct = (trail_peak - pos.entry) * direction / pos.entry * 100 if pos.entry else 0.0
                armed = favor_pct >= risk_pct * cfg.trailing_arm_r
                if armed and direction == 1:
                    trail_peak = max(trail_peak, bar.high)
                    if bar.close <= trail_peak * (1 - cfg.trailing_pct / 100):
                        close_pos(bar.close, i, "trailing_stop")
                        continue
                elif armed and direction == -1:
                    trail_peak = min(trail_peak if trail_peak else bar.low, bar.low)
                    if bar.close >= trail_peak * (1 + cfg.trailing_pct / 100):
                        close_pos(bar.close, i, "trailing_stop")
                        continue

            # 6) 时间阶梯止盈
            if cfg.roi_table:
                target = _roi_target(cfg.roi_table, held_bars * bar_minutes)
                if target is not None and target > 0:
                    profit_pct = (bar.close - pos.entry) * direction / pos.entry * 100
                    # 与实盘 detect_exit 保持同一条规则：低于 roi_min_r 倍风险的
                    # 收益不主动了结。回测少了这一条会把"赢小输大"的单子算成盈利。
                    risk_pct = pos.sl_pct
                    if profit_pct >= target and (
                        cfg.roi_min_r <= 0 or risk_pct <= 0
                        or profit_pct + 1e-9 >= risk_pct * cfg.roi_min_r
                    ):
                        close_pos(bar.close, i, "roi")
                        continue

            # 7) 超时
            chop_limit = cfg.max_hold_minutes_chop if state.get("chop") else 0
            if chop_limit > 0:
                hold_limit_eff = min(hold_limit, cfg.max_hold_minutes, chop_limit)
            else:
                hold_limit_eff = min(hold_limit, cfg.max_hold_minutes)
            if cfg.max_hold_minutes > 0 and held_bars * bar_minutes >= hold_limit_eff:
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
        entry = _fill(next_bar.open, slip, direction, opening=True)
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
        entry_fee = pos.entry_fee
        fee_total += entry_fee
        state = {
            "initial_qty": qty,
            "entry_fee": entry_fee,
            "realized": 0.0,
            "funding": 0.0,
            "partial": False,
            # 入场时的体制：震荡市提前释放资金，与实盘同一条规则
            "chop": entry_regime(features, cfg) == "chop",
        }
        pos.stop_price = entry * (1 - direction * sl_pct / 100)
        # 强平价：逐仓近似 entry × (1 ∓ (1/杠杆 − 维持保证金率))
        lev = max(1.0, float(leverage))
        mmr = max(0.0, float(maintenance_margin_rate))
        state["liq_price"] = entry * (1 - direction * (1.0 / lev - mmr))
        entry_idx = i + 1
        trail_peak = next_bar.open

    # 未平仓的按最后一根收盘强制平掉, 避免收益虚高
    if pos is not None:
        close_pos(candles[-1].close, len(candles) - 1, "timeout")

    result.summary = _summarize(
        result,
        equity,
        initial_equity,
        fee_total,
        max_dd,
        bar_minutes,
        funding_total=funding_total,
        partial_count=partial_count,
        liquidations=liquidations,
        leverage=leverage,
        maintenance_margin_rate=maintenance_margin_rate,
    )
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
    *,
    funding_total: float = 0.0,
    partial_count: int = 0,
    liquidations: int = 0,
    leverage: float = 0.0,
    maintenance_margin_rate: float = 0.0,
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
        "funding_total": round(funding_total, 6),
        "partial_tp_count": partial_count,
        "liquidations": liquidations,
        "leverage": round(float(leverage), 2),
        "maintenance_margin_rate": round(float(maintenance_margin_rate), 5),
        "final_equity": round(equity, 6),
        "disclaimer": (
            "不含 AI 复核与点差闸门; 资金费为假设费率; 强平按维持保证金率估算、跳空按开盘价成交; "
            "仅用于参数粗筛, 不代表实盘收益"
        ),
        **_segments(trades, bar_minutes),
    }
