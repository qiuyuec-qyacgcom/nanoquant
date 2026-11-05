"""分析心法库: 入库 / 去重 / 版本替代 / 衰减排序 / 注入.

设计要点:
- 新旧替代: 新心法与旧心法语义重合且置信度更高时, 新版本生效, 旧条目 superseded_by 指向新条目
- 时间衰减: 超过 ttl_days 未复用的心法自动退出注入 (仍保留可查)
- 注入披露: 返回"哪些心法真的进了 prompt", 避免面板与实况不一致
"""
from __future__ import annotations

import math
import re
import time
from typing import Any

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import Mantra
from ..services.settings import get_evolution
from .guard import audit, sanitize

log = get_logger("evolution.mantra")

_WORD_RE = re.compile(r"[0-9A-Za-z_]+|[\u4e00-\u9fff]")


def _tokens(text: str) -> set[str]:
    chars = _WORD_RE.findall((text or "").lower())
    grams = set(chars)
    cjk = [c for c in chars if len(c) == 1 and "\u4e00" <= c <= "\u9fff"]
    for a, b in zip(cjk, cjk[1:]):
        grams.add(a + b)
    return grams


def similarity(a: str, b: str) -> float:
    ta, tb = _tokens(a), _tokens(b)
    if not ta or not tb:
        return 0.0
    return len(ta & tb) / len(ta | tb)


def health_of(m: Mantra, ttl_days: int) -> float:
    age_days = max(0.0, (time.time() - (m.updated_at or m.created_at)) / 86400.0)
    decay = math.exp(-age_days / max(1.0, ttl_days))
    hit_rate = (m.wins + 1) / (m.hits + 2)
    value = (0.45 * m.confidence + 0.55 * hit_rate) * (0.55 + 0.45 * decay)
    return round(max(0.0, min(1.0, value)), 4)


def serialize(m: Mantra, ttl_days: int | None = None) -> dict[str, Any]:
    ttl = ttl_days or get_evolution().ttl_days
    expired = bool(m.expires_at and m.expires_at < time.time())
    return {
        "id": m.id,
        "title": m.title,
        "rule": m.rule,
        "tags": m.tags or [],
        "category": m.category,
        "source": m.source,
        "confidence": round(m.confidence, 3),
        "health": health_of(m, ttl),
        "hits": m.hits,
        "wins": m.wins,
        "losses": m.losses,
        "enabled": m.enabled,
        "visible": m.visible,
        "version": m.version,
        "superseded_by": m.superseded_by,
        "created_at": m.created_at,
        "updated_at": m.updated_at,
        "expires_at": m.expires_at,
        "expired": expired,
        "evidence": m.evidence or {},
    }


def list_mantras(include_disabled: bool = True, category: str = "", limit: int = 300) -> list[dict[str, Any]]:
    ttl = get_evolution().ttl_days
    with session_scope() as s:
        q = s.query(Mantra)
        if not include_disabled:
            q = q.filter(Mantra.enabled.is_(True))
        if category:
            q = q.filter(Mantra.category == category)
        rows = q.order_by(Mantra.created_at.desc()).limit(limit).all()
        return [serialize(r, ttl) for r in rows]


def rank_lessons(limit: int | None = None) -> list[dict[str, Any]]:
    cfg = get_evolution()
    limit = limit or cfg.max_injected
    now = time.time()
    with session_scope() as s:
        rows = (
            s.query(Mantra)
            .filter(Mantra.enabled.is_(True), Mantra.superseded_by == 0)
            .all()
        )
        scored: list[tuple[float, Mantra]] = []
        for r in rows:
            if r.expires_at and r.expires_at < now:
                continue
            if r.confidence < cfg.min_confidence:
                continue
            scored.append((health_of(r, cfg.ttl_days), r))
        scored.sort(key=lambda x: x[0], reverse=True)
        return [serialize(r, cfg.ttl_days) for _, r in scored[:limit]]


def injection() -> tuple[str, list[dict[str, Any]]]:
    """返回 (注入文本, 注入明细)."""
    cfg = get_evolution()
    if cfg.max_injected <= 0:
        return "", []
    lessons = rank_lessons()
    if not lessons:
        return "", []
    lines = ["【分析心法】(来自历史复盘的沉淀, 按历史有效性排序; 只做概率提示, 不得违反风控铁律)"]
    for i, m in enumerate(lessons, 1):
        lines.append(f"{i}. [{m['category']}] {m['title']} — {m['rule']}")
    detail = [{"id": m["id"], "title": m["title"], "health": m["health"], "category": m["category"]} for m in lessons]
    return "\n".join(lines), detail


def upsert_lesson(
    *,
    title: str,
    rule: str,
    category: str = "entry",
    confidence: float = 0.55,
    evidence: dict | None = None,
    source: str = "trade_review",
    tags: list[str] | None = None,
) -> dict[str, Any]:
    """写入一条候选心法, 返回 {action, mantra, reason}."""
    cfg = get_evolution()
    title, rule = sanitize(title), sanitize(rule)
    ok, why = audit(title, rule, category, confidence)
    if not ok:
        log.info("心法被拦截: %s (%s)", title[:40], why)
        return {"action": "rejected", "reason": why, "title": title}

    now = int(time.time())
    expires = now + cfg.ttl_days * 86400
    with session_scope() as s:
        pool = (
            s.query(Mantra)
            .filter(Mantra.category == category, Mantra.superseded_by == 0)
            .all()
        )
        best: Mantra | None = None
        best_sim = 0.0
        for m in pool:
            sim = similarity(f"{m.title} {m.rule}", f"{title} {rule}")
            if sim > best_sim:
                best, best_sim = m, sim

        if best is not None and best_sim >= 0.62:
            if confidence > best.confidence + 0.03 or len(rule) > len(best.rule) + 20:
                best.superseded_by = 0
                best.enabled = False
                best.updated_at = now
                new = Mantra(
                    title=title, rule=rule, tags=tags or [], category=category,
                    source=source, confidence=confidence, health=confidence,
                    enabled=True, version=best.version + 1,
                    evidence=evidence or {}, created_at=now, updated_at=now, expires_at=expires,
                )
                s.add(new)
                s.flush()
                best.superseded_by = new.id
                return {
                    "action": "replaced",
                    "mantra": serialize(new, cfg.ttl_days),
                    "replaced_id": best.id,
                    "reason": f"与旧心法相似度 {best_sim:.2f}, 新版本置信度更高",
                }
            best.confidence = max(best.confidence, confidence)
            best.hits += 1
            best.updated_at = now
            best.expires_at = expires
            best.evidence = {**(best.evidence or {}), "last_evidence": evidence or {}, "reinforced": (best.evidence or {}).get("reinforced", 0) + 1}
            return {"action": "merged", "mantra": serialize(best, cfg.ttl_days), "reason": f"与旧心法相似度 {best_sim:.2f}, 合并强化"}

        if best is not None and best_sim >= 0.42:
            best.hits += 1
            best.confidence = min(0.95, best.confidence + 0.02)
            best.updated_at = now
            best.expires_at = expires
            return {"action": "reinforced", "mantra": serialize(best, cfg.ttl_days), "reason": f"相似度 {best_sim:.2f}, 相关强化"}

        new = Mantra(
            title=title, rule=rule, tags=tags or [], category=category, source=source,
            confidence=confidence, health=confidence, enabled=True, version=1,
            evidence=evidence or {}, created_at=now, updated_at=now, expires_at=expires,
        )
        s.add(new)
        s.flush()
        return {"action": "created", "mantra": serialize(new, cfg.ttl_days), "reason": "新心法入库"}


def touch(mantra_ids: list[int], won: bool) -> None:
    if not mantra_ids:
        return
    with session_scope() as s:
        for mid in mantra_ids:
            row = s.get(Mantra, mid)
            if not row:
                continue
            row.hits += 1
            if won:
                row.wins += 1
            else:
                row.losses += 1
            if won:
                row.confidence = min(0.98, row.confidence + 0.01)
            else:
                row.confidence = max(0.05, row.confidence - 0.015)


def set_enabled(mantra_id: int, enabled: bool) -> bool:
    with session_scope() as s:
        row = s.get(Mantra, mantra_id)
        if not row:
            return False
        row.enabled = enabled
        row.updated_at = int(time.time())
        return True


def update_mantra(mantra_id: int, patch: dict[str, Any]) -> dict[str, Any] | None:
    with session_scope() as s:
        row = s.get(Mantra, mantra_id)
        if not row:
            return None
        for field in ("title", "rule", "category", "tags"):
            if field in patch and patch[field] is not None:
                setattr(row, field, patch[field])
        if "confidence" in patch and patch["confidence"] is not None:
            row.confidence = max(0.0, min(1.0, float(patch["confidence"])))
        if "enabled" in patch and patch["enabled"] is not None:
            row.enabled = bool(patch["enabled"])
        row.updated_at = int(time.time())
        row.expires_at = row.updated_at + get_evolution().ttl_days * 86400
        return serialize(row)


def retire(mantra_id: int, reason: str = "", by_id: int = 0) -> bool:
    with session_scope() as s:
        row = s.get(Mantra, mantra_id)
        if not row:
            return False
        row.enabled = False
        row.superseded_by = by_id
        row.updated_at = int(time.time())
        evidence = dict(row.evidence or {})
        evidence["retired_reason"] = reason[:300]
        row.evidence = evidence
        return True


def delete_mantra(mantra_id: int) -> bool:
    with session_scope() as s:
        row = s.get(Mantra, mantra_id)
        if not row:
            return False
        s.delete(row)
        return True


def stats() -> dict[str, Any]:
    cfg = get_evolution()
    with session_scope() as s:
        rows = s.query(Mantra).all()
        total = len(rows)
        active = sum(1 for r in rows if r.enabled and not (r.expires_at and r.expires_at < time.time()))
        superseded = sum(1 for r in rows if r.superseded_by)
        by_cat: dict[str, int] = {}
        for r in rows:
            by_cat[r.category] = by_cat.get(r.category, 0) + 1
        avg_conf = sum(r.confidence for r in rows) / total if total else 0.0
        return {
            "total": total,
            "active": active,
            "superseded": superseded,
            "by_category": by_cat,
            "avg_confidence": round(avg_conf, 3),
            "ttl_days": cfg.ttl_days,
            "max_injected": cfg.max_injected,
        }
