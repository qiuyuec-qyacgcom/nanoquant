"""自进化: 逐笔复盘 -> 心法, 定期全局复盘 -> 系统性不足识别.

没有大模型时走确定性规则兜底, 保证自进化闭环永远在转, 只是结论更朴素.
"""
from __future__ import annotations

import time
from typing import Any

from ..ai.client import ai_client
from ..ai.decision import extract_json
from ..ai.prompts import global_review_messages, review_messages
from ..db import session_scope
from ..logging_setup import get_logger
from ..models import EvolutionRun, TradeRecord
from ..services.settings import get_ai, get_evolution
from . import mantra as mantra_store

log = get_logger("evolution.review")


def _trade_payload(t: TradeRecord) -> dict[str, Any]:
    return {
        "symbol": t.symbol,
        "side": t.side,
        "leverage": t.leverage,
        "qty": t.qty,
        "entry_price": t.entry_price,
        "exit_price": t.exit_price,
        "pnl_usdt": round(t.pnl, 6),
        "pnl_pct": round(t.pnl_pct, 4),
        "roi_pct": round(t.roi_pct, 4),
        "fees": round(t.fees, 6),
        "hold_seconds": t.hold_seconds,
        "exit_reason": t.exit_reason,
        "signal_score": t.signal_score,
        "ai_confidence": t.ai_confidence,
        "ai_reason": t.ai_reason[:300],
        "features": t.features or {},
    }


def deterministic_lessons(t: TradeRecord) -> list[dict[str, Any]]:
    """规则兜底: 从出场原因、持仓时长、评分强度反推教训."""
    lessons: list[dict[str, Any]] = []
    f = t.features or {}
    atr_pct = float(f.get("atr_pct") or 0)
    vol_ratio = float(f.get("vol_ratio") or 0)
    score = abs(t.signal_score)
    notional = abs(t.qty * t.entry_price) or 1.0
    fee_ratio = t.fees / max(1e-9, abs(t.pnl) if abs(t.pnl) > 1e-9 else 1e-9)
    if t.fees > 0 and abs(t.pnl) < t.fees * 2.5:
        lessons.append(
            {
                "title": "手续费吞噬大部分盈亏",
                "rule": "当预期波动幅度低于双边手续费的 4 倍 (按当前费率约 0.1%) 时不开仓, 只有 ATR 占比与信号给出的止盈空间同时满足条件才允许下单.",
                "category": "risk",
                "confidence": 0.62,
                "evidence": f"名义额 {notional:.3f}U, 手续费 {t.fees:.5f}U, 净盈亏 {t.pnl:.5f}U",
            }
        )
    if t.pnl < 0 and t.exit_reason in {"manual", "venue_sync"}:
        lessons.append(
            {
                "title": "被动离场说明入场确认不足",
                "rule": f"当信号分绝对值低于 {max(0.45, score):.2f} 或仅单根 K 线确认时, 应等待连续两根同向 K 线收盘价站稳再入场, 避免反转后被动离场.",
                "category": "entry",
                "confidence": 0.56,
                "evidence": f"入场评分 {score:.2f}, 出场方式 {t.exit_reason}, 收益 {t.pnl_pct:.3f}%",
            }
        )
    if t.pnl < 0 and t.exit_reason in {"stop_loss", "trailing_stop"}:
        lessons.append(
            {
                "title": "弱量能下追单易被止损",
                "rule": f"当 1 分钟量比 < 0.9 且 ATR 占比 < 0.25% 时, 即使方向分达到 {score:.2f} 也不开仓, 等待量能放大后再介入.",
                "category": "entry",
                "confidence": 0.58,
                "evidence": f"亏 {t.pnl:.4f}U, 量比 {vol_ratio:.2f}, ATR {atr_pct:.3f}%",
            }
        )
    if t.pnl < 0 and t.hold_seconds < 120:
        lessons.append(
            {
                "title": "开仓 2 分钟内被反向打穿",
                "rule": "当入场后 120 秒内出现反向 0.4% 以上波动时, 视为入场地基不稳, 立即离场并给该标的 15 分钟冷却.",
                "category": "timing",
                "confidence": 0.55,
                "evidence": f"持仓 {t.hold_seconds}s 即触发 {t.exit_reason}",
            }
        )
    if t.pnl > 0 and t.exit_reason == "timeout":
        lessons.append(
            {
                "title": "超时离场仍盈利说明止盈过远",
                "rule": "当浮盈曾超过 1.2% 却在超时离场时, 下次同类信号把止盈目标下调到 1.0% 以内并启用移动止损.",
                "category": "exit",
                "confidence": 0.56,
                "evidence": f"持仓 {t.hold_seconds}s, 收益 {t.pnl_pct:.3f}%",
            }
        )
    if t.pnl > 0 and t.leverage >= 15:
        lessons.append(
            {
                "title": "高杠杆小止损可放大盈亏比",
                "rule": f"当信号分绝对值 > {max(0.4, score):.2f} 且 ATR 占比处于 {max(0.2, atr_pct):.2f}% 附近时, 可用 {int(t.leverage)}x 杠杆配合 1.2 倍 ATR 止损, 但单笔风险不超过权益 8%.",
                "category": "position",
                "confidence": 0.52,
                "evidence": f"{int(t.leverage)}x 杠杆盈利 {t.pnl:.4f}U",
            }
        )
    return lessons[:3]


async def review_trade(trade_id: int, *, force: bool = False) -> dict[str, Any]:
    cfg = get_evolution()
    with session_scope() as s:
        t = s.get(TradeRecord, trade_id)
        if t is None:
            return {"ok": False, "error": "交易不存在"}
        if t.reviewed and not force:
            return {"ok": True, "skipped": True, "reason": "已复盘"}
        s.expunge(t)

    ai_cfg = get_ai()
    existing = [{"title": m["title"], "rule": m["rule"]} for m in mantra_store.rank_lessons(6)]
    verdict = ""
    score = 0
    lessons: list[dict[str, Any]] = []
    used_ai = False
    error = ""

    if ai_cfg.enabled:
        result = await ai_client.chat_with_fallback(
            review_messages(_trade_payload(t), existing),
            purpose="review",
            provider_id=ai_cfg.provider_id,
            fallback_ids=ai_cfg.fallback_provider_ids,
            temperature=0.3,
        )
        if result.ok:
            try:
                data = extract_json(result.text)
                lessons = [x for x in (data.get("lessons") or []) if isinstance(x, dict)]
                verdict = str(data.get("verdict") or "")[:300]
                score = int(data.get("score") or 0)
                used_ai = True
            except ValueError as exc:
                error = str(exc)
        else:
            error = result.error

    if not lessons:
        lessons = deterministic_lessons(t)

    applied: list[dict[str, Any]] = []
    for item in lessons[:3]:
        outcome = mantra_store.upsert_lesson(
            title=str(item.get("title") or ""),
            rule=str(item.get("rule") or ""),
            category=str(item.get("category") or "entry"),
            confidence=float(item.get("confidence") or 0.55),
            evidence={
                "trade_id": t.id,
                "symbol": t.symbol,
                "pnl": round(t.pnl, 6),
                "detail": str(item.get("evidence") or "")[:300],
            },
            source="trade_review",
        )
        applied.append(outcome)

    created_ids = [a["mantra"]["id"] for a in applied if a.get("mantra")]
    with session_scope() as s:
        row = s.get(TradeRecord, trade_id)
        if row:
            row.reviewed = True
            row.review_note = (verdict or ("AI 复盘失败, 使用规则兜底" if not used_ai else ""))[:900]
            row.mantra_ids = created_ids
        s.add(
            EvolutionRun(
                ts=int(time.time()),
                kind="trade_review",
                input_count=1,
                produced=sum(1 for a in applied if a["action"] in {"created", "replaced"}),
                replaced=sum(1 for a in applied if a["action"] == "replaced"),
                ok=True,
                summary=(verdict or f"trade#{trade_id} 复盘完成")[:500],
                raw={"applied": applied, "used_ai": used_ai, "error": error, "score": score},
            )
        )
    return {
        "ok": True,
        "used_ai": used_ai,
        "error": error,
        "verdict": verdict,
        "score": score,
        "applied": [{"action": a["action"], "reason": a.get("reason", ""), "title": (a.get("mantra") or {}).get("title", a.get("title", ""))} for a in applied],
    }


def trade_stats(limit: int = 200, mode: str = "") -> dict[str, Any]:
    """成交统计; 传 mode 时只统计该交易环境 (demo/live)."""
    with session_scope() as s:
        q = s.query(TradeRecord).order_by(TradeRecord.closed_at.desc())
        if mode:
            q = q.filter(TradeRecord.mode == mode)
        rows = q.limit(limit).all()
    total = len(rows)
    if not total:
        return {"trades": 0, "win_rate": 0.0, "pnl": 0.0, "profit_factor": 0.0, "by_reason": {}, "by_symbol": {}, "avg_hold": 0}
    wins = [r for r in rows if r.pnl > 0]
    losses = [r for r in rows if r.pnl <= 0]
    gross_win = sum(r.pnl for r in wins)
    gross_loss = abs(sum(r.pnl for r in losses))
    by_reason: dict[str, dict[str, Any]] = {}
    by_symbol: dict[str, dict[str, Any]] = {}
    for r in rows:
        br = by_reason.setdefault(r.exit_reason or "unknown", {"n": 0, "pnl": 0.0, "wins": 0})
        br["n"] += 1
        br["pnl"] = round(br["pnl"] + r.pnl, 6)
        br["wins"] += 1 if r.pnl > 0 else 0
        bs = by_symbol.setdefault(r.symbol, {"n": 0, "pnl": 0.0, "wins": 0})
        bs["n"] += 1
        bs["pnl"] = round(bs["pnl"] + r.pnl, 6)
        bs["wins"] += 1 if r.pnl > 0 else 0
    return {
        "trades": total,
        "wins": len(wins),
        "losses": len(losses),
        "win_rate": round(len(wins) / total * 100, 2),
        "pnl": round(sum(r.pnl for r in rows), 6),
        "avg_pnl": round(sum(r.pnl for r in rows) / total, 6),
        "profit_factor": round(gross_win / gross_loss, 3) if gross_loss > 0 else (999.0 if gross_win > 0 else 0.0),
        "avg_hold": int(sum(r.hold_seconds for r in rows) / total),
        "by_reason": by_reason,
        "by_symbol": by_symbol,
    }


async def global_review(limit: int = 60) -> dict[str, Any]:
    cfg = get_evolution()
    if not cfg.enabled:
        return {"ok": False, "error": "自进化已关闭"}
    stats = trade_stats(limit)
    if stats.get("trades", 0) < 2:
        return {"ok": False, "error": "样本不足 (至少 2 笔闭环交易)"}
    with session_scope() as s:
        rows = s.query(TradeRecord).order_by(TradeRecord.closed_at.desc()).limit(limit).all()
        trades = [_trade_payload(r) for r in rows]
    existing = mantra_store.rank_lessons(12)
    ai_cfg = get_ai()
    diagnosis: list[str] = []
    applied: list[dict[str, Any]] = []
    retired: list[str] = []
    used_ai = False
    error = ""

    if ai_cfg.enabled:
        result = await ai_client.chat_with_fallback(
            global_review_messages(stats, trades, [{"title": m["title"], "rule": m["rule"], "health": m["health"]} for m in existing]),
            purpose="global_review",
            provider_id=ai_cfg.provider_id,
            fallback_ids=ai_cfg.fallback_provider_ids,
            temperature=0.35,
        )
        if result.ok:
            try:
                data = extract_json(result.text)
                used_ai = True
                diagnosis = [str(x)[:200] for x in (data.get("diagnosis") or [])][:6]
                for item in (data.get("lessons") or [])[:5]:
                    if not isinstance(item, dict):
                        continue
                    applied.append(
                        mantra_store.upsert_lesson(
                            title=str(item.get("title") or ""),
                            rule=str(item.get("rule") or ""),
                            category=str(item.get("category") or "risk"),
                            confidence=float(item.get("confidence") or 0.55),
                            evidence={"source": "global_review", "detail": str(item.get("evidence") or "")[:300], "stats": stats},
                            source="global_review",
                        )
                    )
                for item in (data.get("retire") or [])[:5]:
                    title = str((item or {}).get("title") or "")
                    if not title:
                        continue
                    match = next((m for m in existing if mantra_store.similarity(m["title"], title) >= 0.55), None)
                    if match and mantra_store.retire(match["id"], str((item or {}).get("reason") or "全局复盘淘汰")):
                        retired.append(match["title"])
            except ValueError as exc:
                error = str(exc)
        else:
            error = result.error

    if not used_ai:
        by_reason = stats.get("by_reason", {})
        if stats.get("win_rate", 0) < 45 and stats.get("trades", 0) >= 2:
            applied.append(
                mantra_store.upsert_lesson(
                    title="整体胜率偏低需收紧入场",
                    rule=f"当滚动胜率低于 45% (当前 {stats.get('win_rate', 0):.1f}%) 时, 把开仓阈值提高 0.05 并要求量比同时大于 1.0, 直到胜率回到 50% 以上再恢复.",
                    category="entry",
                    confidence=0.58,
                    evidence={"source": "deterministic_global", "win_rate": stats.get("win_rate"), "trades": stats.get("trades")},
                    source="global_review",
                )
            )
        for reason, data in by_reason.items():
            if data["n"] >= 2 and data["wins"] / data["n"] < 0.5:
                applied.append(
                    mantra_store.upsert_lesson(
                        title=f"{reason} 出场胜率偏低",
                        rule=f"当系统准备以 {reason} 方式离场且当前浮盈低于 0.3% 时, 优先收紧移动止损, 并把该标的列入观察名单, 连续两次同类离场后冷却 30 分钟.",
                        category="exit",
                        confidence=0.55,
                        evidence={"source": "deterministic_global", "stats": data},
                        source="global_review",
                    )
                )
        diagnosis = [f"{k}: {v['n']} 笔, 胜率 {v['wins'] / v['n'] * 100:.0f}%, 净 {v['pnl']:.4f}U" for k, v in by_reason.items()]

    with session_scope() as s:
        s.add(
            EvolutionRun(
                ts=int(time.time()),
                kind="global_review",
                input_count=stats.get("trades", 0),
                produced=sum(1 for a in applied if a["action"] in {"created", "replaced"}),
                replaced=sum(1 for a in applied if a["action"] == "replaced"),
                ok=True,
                summary="; ".join(diagnosis)[:500] or "全局复盘完成",
                raw={"applied": applied, "retired": retired, "stats": stats, "used_ai": used_ai, "error": error},
            )
        )
    return {
        "ok": True,
        "used_ai": used_ai,
        "error": error,
        "diagnosis": diagnosis,
        "applied": [{"action": a["action"], "title": (a.get("mantra") or {}).get("title", ""), "reason": a.get("reason", "")} for a in applied],
        "retired": retired,
        "stats": stats,
    }


def list_runs(limit: int = 50) -> list[dict[str, Any]]:
    with session_scope() as s:
        rows = s.query(EvolutionRun).order_by(EvolutionRun.ts.desc()).limit(limit).all()
        return [
            {
                "id": r.id,
                "ts": r.ts,
                "kind": r.kind,
                "input_count": r.input_count,
                "produced": r.produced,
                "replaced": r.replaced,
                "ok": r.ok,
                "summary": r.summary,
                "raw": r.raw,
            }
            for r in rows
        ]
