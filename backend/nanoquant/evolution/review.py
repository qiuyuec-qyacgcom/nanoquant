"""自进化: 批量台账复盘 -> 心法提案 -> 宪法门禁 -> 证据健康分闭环.

关键纪律（借鉴 astra-quant-agent 的宿主宪章）:
- **不逐笔造心法**：攒够一批未复盘成交才调模型，单笔偶发事件不写长期记忆；
- **NO_CHANGE 是真通道**：模型判定证据不足/没有教训时，代码不回填规则心法；
- **LLM 失败只报事实**：确定性兜底产出带前缀的台账事实，恒 NO_CHANGE、绝不入库；
- 每轮结束按真实平仓证据重算健康分，低分且样本达标才归档，绝不物理删除。
"""
from __future__ import annotations

import time
from typing import Any

from ..ai.client import ai_client
from ..ai.decision import extract_json
from ..ai.prompts import (
    global_review_messages,
    mantra_dedupe_messages,
    review_batch_messages,
)
from ..db import session_scope
from ..logging_setup import get_logger
from ..models import EvolutionRun, PositionRecord, TradeRecord
from ..services.settings import get_ai, get_evolution
from . import guard, health
from . import mantra as mantra_store

log = get_logger("evolution.review")

#: 确定性兜底认知的统一前缀：一眼可辨「这条不是模型说的」。
DETERMINISTIC_PREFIX = "[本地台账推导]"
#: 形成结构性结论所需的最低台账样本。
DETERMINISTIC_MIN_SAMPLE = 10


def _peak_price(trade_id: int) -> float:
    """从仓位表取开仓后的最优价，用于 MFE/MAE 离场质量（TradeRecord 没有该列）."""
    if not trade_id:
        return 0.0
    with session_scope() as s:
        row = (
            s.query(PositionRecord.peak_price)
            .filter(PositionRecord.trade_id == trade_id)
            .first()
        )
    return float(row[0] or 0) if row else 0.0


def _trade_payload(t: TradeRecord, *, compact: bool = False) -> dict[str, Any]:
    features = {k: v for k, v in (t.features or {}).items() if k != "_bars"}
    payload = {
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
        "ai_reason": t.ai_reason[:120] if compact else t.ai_reason[:300],
        # 宿主确定性标注：模型不得对不可观测的快照作因子因果归因
        "snapshot_observability": health.snapshot_observability(features),
    }
    if not compact:
        payload["features"] = features
        quality = health.exit_quality({
            "entry_price": t.entry_price, "exit_price": t.exit_price,
            "peak_price": _peak_price(t.id), "side": t.side,
        })
        payload["exit_quality"] = quality
    return payload


def deterministic_insights(trades: list[Any]) -> list[str]:
    """LLM 全链失败时的兜底认知：只报台账**可观测事实**，零编造、绝不改记忆.

    这些字符串只进 EvolutionRun.raw，不会变成心法；调用方恒置 NO_CHANGE。
    """
    rows = list(trades or [])
    if not rows:
        return [f"{DETERMINISTIC_PREFIX} 台账内没有可观测的已平仓交易，无法形成认知。"]
    nets = [float(getattr(r, "pnl", 0) or 0) for r in rows]
    fees = sum(float(getattr(r, "fees", 0) or 0) for r in rows)
    wins = sum(1 for x in nets if x > 0)
    n = len(nets)
    lines = [
        f"{DETERMINISTIC_PREFIX} 样本 {n} 笔：胜率 {wins / n * 100:.1f}%、"
        f"净盈亏 {sum(nets):+.4f} USDT、手续费 {fees:.4f} USDT。"
    ]
    if n < DETERMINISTIC_MIN_SAMPLE:
        lines.append(
            f"{DETERMINISTIC_PREFIX} 样本仅 {n} 笔（< {DETERMINISTIC_MIN_SAMPLE} 笔），"
            "证据不足，本轮不形成结构性结论。"
        )
        return lines
    by_reason: dict[str, list[float]] = {}
    observability: dict[str, int] = {}
    for r in rows:
        by_reason.setdefault(getattr(r, "exit_reason", "") or "unknown", []).append(float(r.pnl or 0))
        label = health.snapshot_observability(dict(getattr(r, "features", None) or {}))
        observability[label] = observability.get(label, 0) + 1
    dist = "、".join(f"{k} {len(v)} 笔/净 {sum(v):+.3f}U" for k, v in sorted(by_reason.items()))
    lines.append(f"{DETERMINISTIC_PREFIX} 离场机制分布：{dist}。")
    avg_hold = sum(int(getattr(r, "hold_seconds", 0) or 0) for r in rows) / n
    lines.append(f"{DETERMINISTIC_PREFIX} 平均持仓 {avg_hold:.0f} 秒；快照可观测性 {observability}。")
    lines.append(f"{DETERMINISTIC_PREFIX} 以上仅为台账事实，未作因果归因，不代表可执行策略。")
    return lines


def batch_stats(rows: list[Any]) -> dict[str, Any]:
    """批量复盘的统计面板：含证据链标注与「不可据以断言」清单."""
    n = len(rows or [])
    if not n:
        return {"sample_size": 0, "do_not_assert": ["台账为空"]}
    nets = [float(getattr(r, "pnl", 0) or 0) for r in rows]
    wins = sum(1 for x in nets if x > 0)
    by_reason: dict[str, dict[str, Any]] = {}
    observability: dict[str, int] = {}
    quality: list[dict[str, float]] = []
    for r in rows:
        reason = getattr(r, "exit_reason", "") or "unknown"
        cell = by_reason.setdefault(reason, {"n": 0, "pnl": 0.0, "wins": 0})
        cell["n"] += 1
        cell["pnl"] = round(cell["pnl"] + float(r.pnl or 0), 6)
        cell["wins"] += 1 if float(r.pnl or 0) > 0 else 0
        features = dict(getattr(r, "features", None) or {})
        label = health.snapshot_observability(features)
        observability[label] = observability.get(label, 0) + 1
        q = health.exit_quality({
            "entry_price": r.entry_price, "exit_price": r.exit_price,
            "peak_price": _peak_price(r.id), "side": r.side,
        })
        quality.append(q)
    usable = [q for q in quality if q["mfe_pct"] > 0]
    return {
        "sample_size": n,
        "win_rate_pct": round(wins / n * 100, 2),
        "net_pnl": round(sum(nets), 6),
        "fees": round(sum(float(getattr(r, "fees", 0) or 0) for r in rows), 6),
        "avg_hold_seconds": int(sum(int(getattr(r, "hold_seconds", 0) or 0) for r in rows) / n),
        "by_reason": by_reason,
        "snapshot_observability": observability,
        "exit_quality": {
            "avg_mfe_pct": round(sum(q["mfe_pct"] for q in usable) / len(usable), 4) if usable else 0,
            "avg_mae_pct": round(sum(q["mae_pct"] for q in usable) / len(usable), 4) if usable else 0,
            "avg_efficiency_pct": round(sum(q["exit_efficiency_pct"] for q in usable) / len(usable), 2) if usable else 0,
            "covered": len(usable),
        },
        "min_structural_sample": DETERMINISTIC_MIN_SAMPLE,
        "do_not_assert": [
            f"样本 < {DETERMINISTIC_MIN_SAMPLE} 笔的分组结论",
            "PRICE_ONLY / NONE 快照的因子因果归因",
            "单笔极值外推为长期规律",
        ],
    }


def _normalize_change(data: dict[str, Any], lessons: list[dict[str, Any]]) -> str:
    """解析 change_status；兼容旧协议（缺字段时按是否有 lessons 推断）."""
    status = str(data.get("change_status") or "").strip().upper()
    if status in {"NO_CHANGE", "ADD", "REVISE", "INVALIDATE"}:
        return status
    return "ADD" if lessons else "NO_CHANGE"


def _existing_titles() -> list[dict[str, str]]:
    return [
        {"title": m["title"]}
        for m in mantra_store.list_mantras(include_disabled=False, limit=40)
        if not m["superseded_by"]
    ]


async def review_trade(trade_id: int, *, force: bool = False) -> dict[str, Any]:
    """平仓触发点：**只累计样本**，攒够一批才真正复盘，不再逐笔造心法.

    保留旧签名/返回结构（worker 与 API 不变）；``force=True`` 时立即跑一批。
    """
    cfg = get_evolution()
    with session_scope() as s:
        t = s.get(TradeRecord, trade_id)
        if t is None:
            return {"ok": False, "error": "交易不存在"}
        was_reviewed = bool(t.reviewed)
        pnl = float(t.pnl or 0.0)
        injected = list((t.features or {}).get("injected_mantra_ids") or [])
        pending = s.query(TradeRecord).filter(TradeRecord.reviewed.is_(False)).count()

    # 成交胜负回写到决策时注入过的心法（hits/wins/losses 快照）
    if injected and not was_reviewed:
        ids = [int(x) for x in injected if str(x).lstrip("-").isdigit()]
        mantra_store.touch(ids, pnl > 0)

    if not force and pending < cfg.batch_review_size:
        return {
            "ok": True, "skipped": True, "change_status": "NO_CHANGE",
            "reason": f"等待批量样本（{pending}/{cfg.batch_review_size}）",
            "pending": pending, "applied": [], "insights": [],
        }
    return await review_batch(force=force)


async def review_batch(*, limit: int | None = None, force: bool = False) -> dict[str, Any]:
    """批量台账复盘：统计 + 逐笔证据 → change_status → 宪法门禁 → 健康分闭环.

    NO_CHANGE / AI 失败 / 样本不足 都**不写心法**；规则兜底只产出台账事实描述。
    """
    cfg = get_evolution()
    if not cfg.enabled:
        return {"ok": False, "error": "自进化已关闭"}
    size = max(cfg.min_lesson_sample, int(limit or cfg.batch_review_size))
    with session_scope() as s:
        query = s.query(TradeRecord).order_by(TradeRecord.closed_at.desc())
        if not force:
            query = query.filter(TradeRecord.reviewed.is_(False))
        rows = list(query.limit(size).all())

    if len(rows) < cfg.min_lesson_sample:
        return {
            "ok": True, "skipped": True, "change_status": "NO_CHANGE",
            "reason": f"样本不足（{len(rows)}/{cfg.min_lesson_sample}），NO_CHANGE",
            "pending": len(rows), "sample_size": len(rows), "applied": [], "insights": [],
        }

    trade_ids = [r.id for r in rows]
    stats = batch_stats(rows)
    trades_payload = [_trade_payload(r) for r in rows]
    existing = _existing_titles()
    verdict, score = "", 0
    diagnosis: list[str] = []
    lessons: list[dict[str, Any]] = []
    insights: list[str] = []
    used_ai, error = False, ""
    change_status = "NO_CHANGE"

    ai_cfg = get_ai()
    if ai_cfg.enabled:
        result = await ai_client.chat_with_fallback(
            review_batch_messages(stats, trades_payload, existing),
            purpose="review",
            provider_id=ai_cfg.provider_id,
            fallback_ids=ai_cfg.fallback_provider_ids,
            temperature=0.3,
            reasoning_effort="low",
            max_tokens=8000,
        )
        if result.ok:
            try:
                data = extract_json(result.text)
                lessons = [x for x in (data.get("lessons") or []) if isinstance(x, dict)][:3]
                diagnosis = [str(x)[:200] for x in (data.get("diagnosis") or [])][:4]
                verdict = str(data.get("verdict") or "")[:300]
                score = int(data.get("score") or 0)
                used_ai = True
                change_status = _normalize_change(data, lessons)
                if change_status == "NO_CHANGE":
                    lessons = []
            except ValueError as exc:
                error = str(exc)
        else:
            error = result.error

    # LLM 全链失败 / 模型给不出提案：只报台账事实，恒 NO_CHANGE，绝不入库
    if not used_ai:
        change_status, lessons = "NO_CHANGE", []
        insights = deterministic_insights(rows)
    elif not lessons:
        insights = deterministic_insights(rows)

    applied: list[dict[str, Any]] = []
    for item in lessons:
        ok, why = guard.audit_payload(item, sample_size=len(rows))
        if not ok:
            applied.append({"action": "rejected", "reason": why, "title": str(item.get("title") or "")})
            continue
        title, rule = str(item.get("title") or ""), str(item.get("rule") or "")
        disproven = mantra_store.underperforming_similar(title, rule)
        if disproven and not str(item.get("replaces") or ""):
            applied.append({
                "action": "rejected", "title": title,
                "reason": f"与已证伪心法 #{disproven['id']}「{disproven['title']}」语义重复，拒绝新增同类提案",
            })
            continue
        applied.append(
            mantra_store.upsert_lesson(
                title=str(item.get("title") or ""),
                rule=str(item.get("rule") or ""),
                category=str(item.get("category") or "entry"),
                confidence=float(item.get("confidence") or 0.55),
                evidence={
                    "trade_ids": trade_ids[:20],
                    "sample_size": len(rows),
                    "batch_stats": {k: v for k, v in stats.items() if k != "do_not_assert"},
                    "detail": str(item.get("evidence") or "")[:300],
                },
                source="batch_review",
                replaces=str(item.get("replaces") or ""),
                sample_size=len(rows),
            )
        )

    # 旧版本归档留痕（不删除）+ 按真实平仓证据重算健康分/效果反馈
    archived = mantra_store.purge_superseded()
    health_result = mantra_store.refresh_health_from_evidence()
    created_ids = [a["mantra"]["id"] for a in applied if a.get("mantra")]
    note = verdict or ("; ".join(insights) if insights else f"NO_CHANGE（{len(rows)} 笔台账样本）")
    with session_scope() as s:
        for tid in trade_ids:
            row = s.get(TradeRecord, tid)
            if row:
                row.reviewed = True
                row.review_note = note[:900]
                if created_ids:
                    row.mantra_ids = created_ids
        s.add(
            EvolutionRun(
                ts=int(time.time()),
                kind="batch_review",
                input_count=len(rows),
                produced=sum(1 for a in applied if a["action"] in {"created", "replaced"}),
                replaced=sum(1 for a in applied if a["action"] == "replaced"),
                ok=True,
                summary=note[:500],
                raw={
                    "change_status": change_status, "applied": applied, "diagnosis": diagnosis,
                    "insights": insights, "used_ai": used_ai, "error": error, "score": score,
                    "sample_size": len(rows), "stats": stats, "archived": archived,
                    "health": health_result,
                },
            )
        )
    return {
        "ok": True,
        "used_ai": used_ai,
        "error": error,
        "change_status": change_status,
        "sample_size": len(rows),
        "verdict": verdict,
        "score": score,
        "diagnosis": diagnosis,
        "insights": insights,
        "purged": archived,
        "health": health_result,
        "stats": stats,
        "applied": [
            {"action": a["action"], "reason": a.get("reason", ""),
             "title": (a.get("mantra") or {}).get("title", a.get("title", ""))}
            for a in applied
        ],
    }


async def dedupe_mantras(use_ai: bool = True) -> dict[str, Any]:
    """存量心法去重：优先让 AI 判断语义重复，失败时回退字符相似度."""
    # 只给标题/分类/置信度：判断语义重复不需要完整规则，能显著缩短推理型模型的 thinking
    items = [
        {
            "id": m["id"], "title": m["title"],
            "category": m["category"], "confidence": m["confidence"],
        }
        for m in mantra_store.list_mantras(include_disabled=False, limit=40)
        if not m["superseded_by"]
    ]
    if len(items) < 2:
        return {"ok": True, "mode": "none", "kept": len(items), "replaced": [], "reason": "生效心法不足 2 条"}

    ai_error = ""
    replaced: list[dict[str, Any]] = []
    ok_batches = 0
    ai_cfg = get_ai()
    if use_ai and ai_cfg.enabled:
        # 推理型模型对整库一次判断会把 thinking 烧到几千 tokens 并被截断：
        # 按 4 条一批拆分（实测 8 条仍会被 max_tokens 截断），单批失败不影响其它批。
        batches = [items[i:i + 4] for i in range(0, len(items), 4)]
        for batch in batches:
            batch_ids = {m["id"]: m for m in batch}
            result = await ai_client.chat_with_fallback(
                mantra_dedupe_messages(batch),
                purpose="mantra_dedupe",
                provider_id=ai_cfg.provider_id,
                fallback_ids=ai_cfg.fallback_provider_ids,
                temperature=0.1,
                reasoning_effort="low",
                max_tokens=3000,
            )
            if not result.ok:
                ai_error = result.error
                log.warning("AI 心法去重单批失败: %s", result.error[:160])
                continue
            try:
                data = extract_json(result.text)
            except ValueError as exc:
                ai_error = str(exc)
                log.warning("AI 心法去重解析失败: %s", exc)
                continue
            ok_batches += 1
            for group in (data.get("merge") or [])[:10]:
                if not isinstance(group, dict):
                    continue
                try:
                    keep_id = int(group.get("keep") or 0)
                except (TypeError, ValueError):
                    continue
                keep = batch_ids.get(keep_id)
                if keep is None:
                    continue
                for raw in group.get("replaced") or []:
                    try:
                        old_id = int(raw)
                    except (TypeError, ValueError):
                        continue
                    if old_id == keep_id or old_id not in batch_ids:
                        continue
                    if mantra_store.retire(
                        old_id,
                        f"AI 去重：并入 #{keep_id} {keep['title'][:40]}",
                        by_id=keep_id,
                    ):
                        replaced.append({"id": old_id, "kept_id": keep_id, "kept_title": keep["title"]})

    if ok_batches:
        return {
            "ok": True,
            "mode": "ai" if not ai_error else "ai_partial",
            "kept": len(items) - len(replaced),
            "replaced": replaced,
            "error": ai_error,
        }

    fallback = mantra_store.dedupe()
    return {**fallback, "mode": "rule", "error": ai_error}


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
    if stats.get("trades", 0) < cfg.min_lesson_sample:
        return {"ok": False, "error": f"样本不足 (至少 {cfg.min_lesson_sample} 笔闭环交易)"}
    with session_scope() as s:
        rows = s.query(TradeRecord).order_by(TradeRecord.closed_at.desc()).limit(limit).all()
        trades = [_trade_payload(r, compact=True) for r in rows]
    existing = [
        m for m in mantra_store.list_mantras(include_disabled=False, limit=40) if not m["superseded_by"]
    ]
    ai_cfg = get_ai()
    diagnosis: list[str] = []
    applied: list[dict[str, Any]] = []
    retired: list[str] = []
    used_ai = False
    error = ""
    change_status = "NO_CHANGE"
    sample_size = int(stats.get("trades", 0))

    if ai_cfg.enabled:
        result = await ai_client.chat_with_fallback(
            global_review_messages(stats, trades, [{"title": m["title"], "health": m["health"]} for m in existing]),
            purpose="global_review",
            provider_id=ai_cfg.provider_id,
            fallback_ids=ai_cfg.fallback_provider_ids,
            temperature=0.35,
            reasoning_effort="low",
            max_tokens=8000,
        )
        if result.ok:
            try:
                data = extract_json(result.text)
                used_ai = True
                diagnosis = [str(x)[:200] for x in (data.get("diagnosis") or [])][:6]
                proposals = [x for x in (data.get("lessons") or []) if isinstance(x, dict)][:5]
                change_status = _normalize_change(data, proposals)
                for item in ([] if change_status == "NO_CHANGE" else proposals):
                    ok, why = guard.audit_payload(item, sample_size=sample_size)
                    if not ok:
                        applied.append({"action": "rejected", "reason": why, "title": str(item.get("title") or "")})
                        continue
                    disproven = mantra_store.underperforming_similar(
                        str(item.get("title") or ""), str(item.get("rule") or "")
                    )
                    if disproven and not str(item.get("replaces") or ""):
                        applied.append({
                            "action": "rejected", "title": str(item.get("title") or ""),
                            "reason": f"与已证伪心法 #{disproven['id']} 语义重复，拒绝新增同类提案",
                        })
                        continue
                    applied.append(
                        mantra_store.upsert_lesson(
                            title=str(item.get("title") or ""),
                            rule=str(item.get("rule") or ""),
                            category=str(item.get("category") or "risk"),
                            confidence=float(item.get("confidence") or 0.55),
                            evidence={
                                "source": "global_review", "sample_size": sample_size,
                                "detail": str(item.get("evidence") or "")[:300], "stats": stats,
                            },
                            source="global_review",
                            replaces=str(item.get("replaces") or ""),
                            sample_size=sample_size,
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

    # LLM 全链失败：只把台账事实写进报告，恒 NO_CHANGE、绝不回填规则心法
    if not used_ai:
        change_status, applied = "NO_CHANGE", []
        by_reason = stats.get("by_reason", {})
        diagnosis = [
            f"{DETERMINISTIC_PREFIX} {k}: {v['n']} 笔, 胜率 {v['wins'] / v['n'] * 100:.0f}%, 净 {v['pnl']:.4f}U"
            for k, v in by_reason.items()
        ]
    if change_status == "NO_CHANGE":
        applied = []

    archived = mantra_store.purge_superseded()
    health_result = mantra_store.refresh_health_from_evidence()
    with session_scope() as s:
        s.add(
            EvolutionRun(
                ts=int(time.time()),
                kind="global_review",
                input_count=stats.get("trades", 0),
                produced=sum(1 for a in applied if a["action"] in {"created", "replaced"}),
                replaced=sum(1 for a in applied if a["action"] == "replaced"),
                ok=True,
                summary=("; ".join(diagnosis)[:500] or f"全局复盘 NO_CHANGE（{stats.get('trades', 0)} 笔样本）"),
                raw={
                    "change_status": change_status, "applied": applied, "retired": retired,
                    "stats": stats, "used_ai": used_ai, "error": error,
                    "archived": archived, "health": health_result,
                },
            )
        )
    return {
        "ok": True,
        "used_ai": used_ai,
        "error": error,
        "change_status": change_status,
        "diagnosis": diagnosis,
        "applied": [{"action": a["action"], "title": (a.get("mantra") or {}).get("title", ""), "reason": a.get("reason", "")} for a in applied],
        "retired": retired,
        "purged": archived,
        "health": health_result,
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
