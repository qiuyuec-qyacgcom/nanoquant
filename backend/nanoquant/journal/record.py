"""决策留痕: 每次 AI 给出方向判断时写一份快照.

只记录 AI 参与的方向性判断（open_long / open_short），hold 只留痕不参与对赌。
与 Signal（每轮扫描信号）不同：这是「决策」级别，且带预测方向 + 目标 + 快照。
"""
from __future__ import annotations

import time
from typing import Any

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import DecisionJournal
from ..services.settings import get_journal

log = get_logger("journal.record")

_DIRECTION = {"open_long": "long", "open_short": "short", "hold": "flat", "close": "flat"}


def record_decision(
    *,
    symbol: str,
    action: str,
    confidence: float,
    entry_price: float,
    target_pct: float,
    reason: str,
    snapshot: dict[str, Any] | None,
    signal_id: int = 0,
    injected_experience_ids: list[int] | None = None,
    mode: str = "demo",
) -> int | None:
    """写入一条决策快照, 返回其 id; 关闭或低于记录门槛时返回 None.

    ``target_pct`` 为**方向校准后**的预测涨跌幅：long 为正、short 为负（调用方负责符号）。
    """
    cfg = get_journal()
    if not cfg.enabled:
        return None
    direction = _DIRECTION.get(action, "flat")
    if direction == "flat":
        return None
    if confidence < cfg.min_record_confidence:
        return None
    with session_scope() as s:
        row = DecisionJournal(
            ts=int(time.time()),
            symbol=symbol,
            direction=direction,
            confidence=round(float(confidence), 4),
            entry_price=float(entry_price or 0.0),
            target_pct=round(float(target_pct or 0.0), 4),
            reason=(reason or "")[:900],
            snapshot=snapshot or {},
            decision=action,
            signal_id=signal_id,
            injected_experience_ids=list(injected_experience_ids or []),
            status="pending",
            mode=mode,
        )
        s.add(row)
        s.flush()
        return row.id


def _serialize(r: DecisionJournal) -> dict[str, Any]:
    return {
        "id": r.id,
        "ts": r.ts,
        "symbol": r.symbol,
        "direction": r.direction,
        "confidence": r.confidence,
        "entry_price": r.entry_price,
        "target_pct": r.target_pct,
        "reason": r.reason,
        "snapshot": r.snapshot or {},
        "decision": r.decision,
        "signal_id": r.signal_id,
        "injected_experience_ids": r.injected_experience_ids or [],
        "status": r.status,
        "outcome": r.outcome,
        "actual_price": r.actual_price,
        "actual_pct": r.actual_pct,
        "labeled_at": r.labeled_at,
        "archived": r.archived,
        "mode": r.mode,
    }


def list_journal(
    *,
    limit: int = 200,
    status: str = "",
    symbol: str = "",
    mode: str = "",
) -> list[dict[str, Any]]:
    with session_scope() as s:
        q = s.query(DecisionJournal).order_by(DecisionJournal.ts.desc())
        if status:
            q = q.filter(DecisionJournal.status == status)
        if symbol:
            q = q.filter(DecisionJournal.symbol == symbol)
        if mode:
            q = q.filter(DecisionJournal.mode == mode)
        rows = q.limit(limit).all()
        return [_serialize(r) for r in rows]


def journal_stats(mode: str = "") -> dict[str, Any]:
    with session_scope() as s:
        q = s.query(DecisionJournal)
        if mode:
            q = q.filter(DecisionJournal.mode == mode)
        total = q.count()
        pending = q.filter(DecisionJournal.status == "pending").count()
        labeled = q.filter(DecisionJournal.status == "labeled").count()
        correct = q.filter(DecisionJournal.outcome == "correct").count()
        wrong = q.filter(DecisionJournal.outcome == "wrong").count()
        partial = q.filter(DecisionJournal.outcome == "partial").count()
    judged = correct + wrong
    return {
        "total": total,
        "pending": pending,
        "labeled": labeled,
        "correct": correct,
        "wrong": wrong,
        "partial": partial,
        "accuracy": round(correct / judged * 100, 2) if judged else 0.0,
    }
