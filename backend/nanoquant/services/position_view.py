"""持仓派生字段 (单一实现, 供 status 与 positions 两个接口共用).

避免"同一个仓位在两个接口里算法不一致"这类脏 bug.
"""
from __future__ import annotations

from typing import Any

MMR = 0.005  # 维持保证金率近似值, 仅用于估算强平价


def derived(position: Any) -> dict[str, Any]:
    direction = 1 if position.side == "long" else -1
    entry = float(position.entry_price or 0)
    mark = float(position.mark_price or entry)
    qty = float(position.qty or 0)
    margin = float(position.margin or 0)

    unrealized = (mark - entry) * qty * direction
    notional = qty * mark
    roi = (unrealized / margin * 100) if margin else 0.0

    leverage = max(1.0, float(position.leverage or 1))
    if not entry:
        liq = 0.0
    elif position.side == "long":
        liq = max(0.0, entry * (1 - 1 / leverage + MMR))
    else:
        liq = entry * (1 + 1 / leverage - MMR)

    partial_at = int(getattr(position, "partial_taken_at", 0) or 0)
    return {
        "unrealized": round(unrealized, 8),
        "notional": round(notional, 6),
        "roi_pct": round(roi, 3),
        "liquidation_price": round(liq, 8),
        "liquidation_estimated": True,
        # 部分止盈标记: 前端据此显示「已部分止盈」并提示剩余仓位仍受移动止损保护
        "partial_taken_at": partial_at,
        "partial_tp": partial_at > 0,
    }
