"""24h 回访打标: 把预测方向 vs 实际价格对齐成 correct / wrong / partial.

方向校准后的实际涨跌幅 (signed actual)：
- long 预测且实际 +x% → 方向正确；short 预测且实际 -x% → 方向正确（取负号后仍为正）。
- 达到 ``correct_threshold_pct`` 判对，反向达到判错，中间为 partial。
"""
from __future__ import annotations

import time
from typing import Any

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import DecisionJournal
from ..services.settings import get_journal

log = get_logger("journal.label")

_DIRECTIONAL = ("long", "short")


def signed_actual_pct(direction: str, entry_price: float, actual_price: float) -> float:
    """方向校准后的涨跌幅：long 记正、short 记负（均以盈利为正）。"""
    if not entry_price or entry_price <= 0:
        return 0.0
    raw = (actual_price - entry_price) / entry_price * 100.0
    return raw if direction == "long" else -raw


def outcome_of(signed_pct: float, threshold: float) -> str:
    if signed_pct >= threshold:
        return "correct"
    if signed_pct <= -threshold:
        return "wrong"
    return "partial"


async def label_due() -> dict[str, Any]:
    """回访所有到期的 pending 方向性决策并打标.

    按 symbol 分组取一次价，减少 REST 调用；单个标的取价失败不阻塞其余。
    """
    cfg = get_journal()
    if not cfg.enabled:
        return {"ok": False, "error": "决策日志已关闭", "labeled": 0}
    now = int(time.time())
    cutoff = now - cfg.label_after_sec
    with session_scope() as s:
        rows = (
            s.query(DecisionJournal)
            .filter(
                DecisionJournal.status == "pending",
                DecisionJournal.direction.in_(_DIRECTIONAL),
                DecisionJournal.ts <= cutoff,
            )
            .limit(200)
            .all()
        )
        ids = [(r.id, r.symbol) for r in rows]
    if not ids:
        return {"ok": True, "labeled": 0, "due": 0}

    from ..exchange import client as binance_client

    symbols = sorted({sym for _, sym in ids})
    prices: dict[str, float] = {}
    try:
        tickers = await binance_client.client.tickers(symbols)
        prices = {sym: float(t.last) for sym, t in tickers.items() if t and t.last}
    except Exception as exc:  # noqa: BLE001
        log.warning("回访取价失败: %s", exc)
        return {"ok": False, "error": str(exc)[:160], "labeled": 0, "due": len(ids)}

    labeled = 0
    with session_scope() as s:
        for rid, sym in ids:
            price = prices.get(sym)
            if not price:
                continue
            row = s.get(DecisionJournal, rid)
            if row is None or row.status != "pending":
                continue
            signed = signed_actual_pct(row.direction, row.entry_price, price)
            row.actual_price = price
            row.actual_pct = round(signed, 4)
            row.outcome = outcome_of(signed, cfg.correct_threshold_pct)
            row.labeled_at = now
            row.status = "labeled"
            labeled += 1
    return {"ok": True, "labeled": labeled, "due": len(ids)}
