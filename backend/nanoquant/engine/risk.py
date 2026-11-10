"""风控闸门与仓位计算.

所有拒绝都带可读原因, 会写进 signals.reject_reason, 便于面板直接展示
"为什么这一轮没开仓".
"""
from __future__ import annotations

import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Any

from ..db import session_scope
from ..exchange.broker import Account, estimate_qty
from ..exchange.precision import SymbolFilters
from ..logging_setup import get_logger
from ..models import PositionRecord, TradeRecord
from ..services.settings import EngineSettings, RiskSettings, get_engine, normalize_mode
from ..services import state

log = get_logger("risk")


@dataclass
class GateDecision:
    ok: bool
    reason: str = ""
    warnings: list[str] | None = None


def _now_local(cfg: EngineSettings) -> datetime:
    return datetime.now(timezone.utc) + timedelta(hours=cfg.timezone_offset)


def trading_window_open(cfg: EngineSettings) -> tuple[bool, str]:
    windows = [w for w in (cfg.trading_windows or []) if w.strip()]
    if not windows:
        return True, ""
    now = _now_local(cfg)
    minutes = now.hour * 60 + now.minute
    for raw in windows:
        try:
            start_s, end_s = raw.split("-")
            sh, sm = (int(x) for x in start_s.split(":"))
            eh, em = (int(x) for x in end_s.split(":"))
        except ValueError:
            continue
        start = sh * 60 + sm
        end = eh * 60 + em
        if end == 24 * 60:
            end = 24 * 60
        if start <= end:
            if start <= minutes < end:
                return True, ""
        else:  # 跨日窗口
            if minutes >= start or minutes < end:
                return True, ""
    return False, f"当前不在交易时段 ({', '.join(windows)})"


def ensure_day_anchor(equity: float, cfg: EngineSettings) -> float:
    """按交易时区做日切, 返回当日初始权益."""
    today = _now_local(cfg).strftime("%Y-%m-%d")
    anchor_day = state.get("day_anchor")
    if anchor_day != today or not state.get("day_start_equity"):
        state.set_many(
            {
                "day_anchor": today,
                "day_start_equity": f"{equity:.8f}",
                "equity_peak": f"{max(equity, state.get_float('equity_peak', equity)):.8f}",
            }
        )
        return equity
    return state.get_float("day_start_equity", equity)


def update_peak(equity: float) -> float:
    peak = max(equity, state.get_float("equity_peak", equity))
    if abs(peak - state.get_float("equity_peak", equity)) > 1e-12:
        state.set("equity_peak", f"{peak:.8f}")
    return peak


def open_positions(mode: str | None = None) -> list[PositionRecord]:
    """当前交易环境下的在仓记录.

    模拟盘与实盘仓位互不相干: 计数、敞口与同标的去重都只看当前环境,
    否则切到实盘后模拟盘仓位会占用 max_positions 与敞口额度.
    """
    resolved = normalize_mode(mode or get_engine().mode)
    with session_scope() as s:
        return (
            s.query(PositionRecord)
            .filter(PositionRecord.status == "open", PositionRecord.mode == resolved)
            .all()
        )


def side_cap_reason(positions: list[PositionRecord], side: str, limit: int) -> str:
    """同向集中控制: 所有币同涨同跌, 同向多仓本质是加杠杆.

    limit<=0 表示不限制 (默认, 保持旧行为); 建议小资金设为 1.
    """
    if limit <= 0 or not side:
        return ""
    same = sum(1 for p in positions if (getattr(p, "side", "") or "") == side)
    if same >= limit:
        return f"同方向持仓已达上限 {limit} 个 ({side}), 避免相关性叠加"
    return ""


def vol_risk_scale(target_atr_pct: float, atr_pct: float, *, low: float = 0.25, high: float = 2.0) -> float:
    """波动率目标仓位缩放: ATR% 高则缩小风险预算, 低则放大 (限制在 [low, high]).

    target<=0 或 atr 无效时返回 1.0, 即关闭该功能.
    """
    if target_atr_pct <= 0 or atr_pct <= 0:
        return 1.0
    return max(low, min(high, target_atr_pct / atr_pct))


def total_notional(prices: dict[str, float]) -> float:
    total = 0.0
    for p in open_positions():
        px = prices.get(p.symbol, p.mark_price or p.entry_price)
        total += p.qty * px
    return total


def global_gates(account: Account, cfg_engine: EngineSettings, cfg_risk: RiskSettings) -> GateDecision:
    if account.equity <= 0:
        return GateDecision(False, "账户权益为 0")
    if account.equity < cfg_risk.min_equity_usdt:
        return GateDecision(False, f"权益 {account.equity:.4f} USDT 低于下限 {cfg_risk.min_equity_usdt} USDT")
    day_start = ensure_day_anchor(account.equity, cfg_engine)
    peak = update_peak(account.equity)
    if day_start > 0:
        day_pnl_pct = (account.equity - day_start) / day_start * 100
        if day_pnl_pct <= -cfg_risk.daily_loss_limit_pct:
            return GateDecision(False, f"触发日内亏损上限 {day_pnl_pct:.2f}% (限 {cfg_risk.daily_loss_limit_pct}%)")
    if peak > 0:
        dd = (peak - account.equity) / peak * 100
        if dd >= cfg_risk.max_drawdown_pct:
            return GateDecision(False, f"触发最大回撤限制 {dd:.2f}% (限 {cfg_risk.max_drawdown_pct}%)")
    losses = state.get_int("consecutive_losses", 0)
    if losses >= cfg_risk.consecutive_loss_limit:
        halt_until = state.get_float("loss_halt_until", 0)
        if time.time() < halt_until:
            remain = int(halt_until - time.time())
            return GateDecision(False, f"连亏 {losses} 笔, 冷静期剩余 {remain}s")
        state.set("consecutive_losses", "0")
    guard_until = state.get_float("sl_guard_until", 0)
    if guard_until > time.time():
        remain = int(guard_until - time.time())
        return GateDecision(False, f"止损熔断中, 剩余 {remain}s (窗口内多次止损)")
    halted = state.get("halted")
    if halted:
        return GateDecision(False, f"引擎已熔断: {halted}")
    return GateDecision(True)


def symbol_gates(
    symbol: str,
    account: Account,
    cfg_engine: EngineSettings,
    cfg_risk: RiskSettings,
    *,
    prices: dict[str, float],
    side: str = "",
) -> GateDecision:
    positions = open_positions()
    if any(p.symbol == symbol for p in positions):
        return GateDecision(False, "该标的已有持仓")
    if len(positions) >= cfg_engine.max_positions:
        return GateDecision(False, f"持仓数已达上限 {cfg_engine.max_positions}")
    cap = side_cap_reason(positions, side, cfg_risk.max_same_direction)
    if cap:
        return GateDecision(False, cap)
    cooldowns = state.active_cooldowns(int(time.time()))
    cd = cooldowns.get(symbol)
    if cd:
        return GateDecision(False, f"标的冷却中, 剩余 {cd['remaining']}s: {cd['reason']}")
    notional = total_notional(prices) + 0.0
    exposure_pct = notional / account.equity * 100 if account.equity > 0 else 0.0
    if exposure_pct >= cfg_risk.max_exposure_pct:
        return GateDecision(False, f"总敞口 {exposure_pct:.1f}% 已达上限 {cfg_risk.max_exposure_pct}%")
    return GateDecision(True)


def choose_leverage(symbol: str, cfg_engine: EngineSettings, cfg_risk: RiskSettings, filters: SymbolFilters, ai_leverage: float = 0.0) -> int:
    """开仓杠杆统一由「参数配置 → 风控」的**全局杠杆下限/上限**决定.

    - 上限 = min(全局杠杆上限, 该标的交易所上限, 100)；
    - 下限 = min(全局杠杆下限, 上限)：交易所上限更低时以交易所为准（安全优先）；
    - AI（含注入心法影响）或外部信号给出的杠杆只是区间内的**建议值**，
      低于下限抬到下限、高于上限压到上限；心法文本里的杠杆数值不参与决策；
    - 没有建议值时直接用下限（原「默认杠杆」语义）。

    ``cfg_engine`` 保留在签名中以兼容既有调用，杠杆不受引擎分组参数干预。
    """
    upper = max(1, min(int(cfg_risk.max_leverage), int(filters.max_leverage or 100), 100))
    lower = max(1, min(int(cfg_risk.default_leverage), upper))
    desired = float(ai_leverage) if ai_leverage and ai_leverage > 0 else float(lower)
    return int(max(lower, min(round(desired), upper)))


def plan_position(
    *,
    price: float,
    filters: SymbolFilters,
    account: Account,
    cfg_risk: RiskSettings,
    sl_pct: float,
    leverage: int,
    risk_scale: float = 1.0,
    available: float | None = None,
) -> tuple[float, dict[str, Any]]:
    """available 传引擎的「本轮剩余额度」；为 None 时退回账户的可用余额."""
    effective_available = account.available if available is None else available
    qty, note = estimate_qty(
        account.equity, price, leverage, cfg_risk, sl_pct / 100.0, filters,
        risk_scale=risk_scale, available=effective_available,
    )
    notional = qty * price
    info = {
        "qty": qty,
        "notional": round(notional, 6),
        "margin": round(notional / max(1, leverage), 6),
        "leverage": leverage,
        "available": round(effective_available, 6),
        "risk_scale": round(risk_scale, 3),
        "note": note,
        "effective_min_notional": filters.effective_min_notional(price),
        "min_qty": filters.effective_min_qty(price),
    }
    return qty, info


def register_close(pnl: float, symbol: str, cfg_risk: RiskSettings, exit_reason: str = "") -> None:
    if pnl < 0:
        losses = state.get_int("consecutive_losses", 0) + 1
        state.set("consecutive_losses", str(losses))
        if cfg_risk.symbol_cooldown_minutes > 0:
            state.set_cooldown(
                symbol,
                int(time.time()) + cfg_risk.symbol_cooldown_minutes * 60,
                f"本笔亏损 {pnl:.4f} USDT",
            )
        if losses >= cfg_risk.consecutive_loss_limit:
            halt_sec = max(600, cfg_risk.cooldown_minutes * 60)
            state.set("loss_halt_until", str(int(time.time()) + halt_sec))
    else:
        state.set("consecutive_losses", "0")
        state.clear_cooldown(symbol)

    # 止损熔断 (StoplossGuard): 止损/移动止损在窗口内达到阈值 → 全局暂停开仓
    if exit_reason in ("stop_loss", "trailing_stop") and cfg_risk.stoploss_guard_count > 0:
        mode = normalize_mode(get_engine().mode)
        since = int(time.time()) - cfg_risk.stoploss_guard_window_min * 60
        with session_scope() as s:
            count = (
                s.query(TradeRecord)
                .filter(
                    TradeRecord.mode == mode,
                    TradeRecord.exit_reason.in_(("stop_loss", "trailing_stop")),
                    TradeRecord.closed_at >= since,
                )
                .count()
            )
        if count >= cfg_risk.stoploss_guard_count:
            until = int(time.time()) + cfg_risk.stoploss_guard_pause_min * 60
            state.set("sl_guard_until", str(until))
            log.warning(
                "止损熔断: %d 分钟内 %d 次止损, 暂停开仓 %d 分钟",
                cfg_risk.stoploss_guard_window_min,
                count,
                cfg_risk.stoploss_guard_pause_min,
            )
