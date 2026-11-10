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

#: 自动替代阈值：标题 + 规则加权相似度
REPLACE_SIM = 0.48
#: 合并强化阈值（低于替代，只累加命中/微调置信度）
MERGE_SIM = 0.32
#: 替代冷却：刚入库且从未被验证的心法先不替代，避免同一晚反复翻烧饼
REPLACE_COOLDOWN_SEC = 6 * 3600


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


def lesson_similarity(old_title: str, old_rule: str, title: str, rule: str) -> float:
    """标题 + 规则加权相似度.

    只看整段文本时，中文近义改写（"回踩确认" vs "双向确认"）会被稀释到 0.4 以下，
    导致语义重复的心法不断新建、旧版本永远不替代。标题权重更高。
    """
    return round(
        0.35 * similarity(old_title, title) + 0.65 * similarity(old_rule, rule), 4
    )


def _can_replace(old: Mantra, confidence: float, rule: str, now: int) -> bool:
    """是否允许新版本替代旧心法.

    - 6 小时内刚入库且一次都没被注入验证过：只合并，不替代（防抖动）；
    - 置信度不低于旧版本 0.05，或规则明显更具体（长 15% 以上）。
    """
    if now - int(old.created_at or 0) < REPLACE_COOLDOWN_SEC and int(old.hits or 0) == 0:
        return False
    return confidence >= float(old.confidence) - 0.05 or len(rule) >= len(old.rule) * 1.15


def health_of(m: Mantra, ttl_days: int) -> float:
    """未经过证据重算时的回退分（只作展示/冷启动，不参与质量决策）."""
    age_days = max(0.0, (time.time() - (m.updated_at or m.created_at)) / 86400.0)
    decay = math.exp(-age_days / max(1.0, ttl_days))
    hit_rate = (m.wins + 1) / (m.hits + 2)
    value = (0.45 * m.confidence + 0.55 * hit_rate) * (0.55 + 0.45 * decay)
    return round(max(0.0, min(1.0, value)), 4)


def effective_health(m: Mantra) -> float:
    """证据重算过的用重算分；没打过分的排在有分的后面，不冒充健康。"""
    evidence = m.evidence if isinstance(m.evidence, dict) else {}
    if evidence.get("health_basis"):
        return round(max(0.0, min(1.0, float(m.health or 0.0))), 4)
    return health_of(m, get_evolution().ttl_days) * 0.5


def serialize(m: Mantra, ttl_days: int | None = None) -> dict[str, Any]:
    ttl = ttl_days or get_evolution().ttl_days
    expired = bool(m.expires_at and m.expires_at < time.time())
    evidence = m.evidence if isinstance(m.evidence, dict) else {}
    basis = evidence.get("health_basis") or {}
    return {
        "id": m.id,
        "title": m.title,
        "rule": m.rule,
        "tags": m.tags or [],
        "category": m.category,
        "source": m.source,
        "confidence": round(m.confidence, 3),
        "health": effective_health(m),
        "health_source": "evidence" if basis else "fallback",
        "sample_size": basis.get("sample_size", 0),
        "basis": basis.get("basis", ""),
        "archived": bool(evidence.get("archived_reason")),
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


def underperforming_similar(title: str, rule: str) -> dict[str, Any] | None:
    """效果评估判定「采用组更差」的同类心法：生成侧据此拒绝语义重复的新提案."""
    with session_scope() as s:
        rows = s.query(Mantra).filter(Mantra.enabled.is_(True), Mantra.superseded_by == 0).all()
        for m in rows:
            evidence = m.evidence if isinstance(m.evidence, dict) else {}
            if evidence.get("effectiveness_verdict") != "underperform":
                continue
            if lesson_similarity(m.title, m.rule, title, rule) >= MERGE_SIM:
                return {"id": m.id, "title": m.title}
    return None


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
            health = effective_health(r)
            evidence = r.evidence if isinstance(r.evidence, dict) else {}
            # 效果评估反馈：被判「采用组更差」的心法降权，不能靠新/靠模型自报置信度挤进前排
            if evidence.get("effectiveness_verdict") == "underperform":
                health *= 0.8
            scored.append((health, r))
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
    detail = [
        {"id": m["id"], "title": m["title"], "health": m["health"], "category": m["category"],
         "health_source": m["health_source"], "sample_size": m["sample_size"]}
        for m in lessons
    ]
    return "\n".join(lines), detail


def injection_report() -> dict[str, Any]:
    """披露注入实况：生效 / 实际注入 / 未注入清单（与 injection 共用同一份排序）."""
    cfg = get_evolution()
    now = time.time()
    with session_scope() as s:
        rows = s.query(Mantra).filter(Mantra.enabled.is_(True), Mantra.superseded_by == 0).all()
        active = [r for r in rows if not (r.expires_at and r.expires_at < now)]
        ranked = rank_lessons(limit=len(active) + 1)
    injected = ranked[: cfg.max_injected]
    injected_ids = {m["id"] for m in injected}
    return {
        "active": len(active),
        "injected": len(injected),
        "limit": cfg.max_injected,
        "not_injected": [
            {"id": m["id"], "title": m["title"], "health": m["health"]}
            for m in ranked if m["id"] not in injected_ids
        ],
        "unscored": sum(1 for m in ranked if m["health_source"] != "evidence"),
    }


def upsert_lesson(
    *,
    title: str,
    rule: str,
    category: str = "entry",
    confidence: float = 0.55,
    evidence: dict | None = None,
    source: str = "trade_review",
    tags: list[str] | None = None,
    replaces: str = "",
    sample_size: int | None = None,
    expected_version: str | None = None,
) -> dict[str, Any]:
    """写入一条候选心法, 返回 {action, mantra, reason}.

    ``sample_size`` 为支持该结论的独立平仓样本数；None 表示管理员手工录入（豁免样本门槛）。
    ``expected_version`` 提供时走乐观并发校验，版本不一致直接放弃，避免覆盖人工编辑。
    """
    cfg = get_evolution()
    title, rule = sanitize(title), sanitize(rule)
    ok, why = audit(title, rule, category, confidence, sample_size=sample_size)
    if not ok:
        log.info("心法被拦截: %s (%s)", title[:40], why)
        return {"action": "rejected", "reason": why, "title": title}

    from ..services import state  # 延迟导入，避免模块初始化顺序问题

    current_version = state.get("mantra_version", "0")
    if expected_version is not None and expected_version != current_version:
        return {"action": "conflict", "reason": "心法库版本已变化，请重新加载后再操作", "title": title}

    now = int(time.time())
    expires = now + cfg.ttl_days * 86400
    with session_scope() as s:
        # 已生效的全部心法（跨 category，AI 点名替代时也要能找到）
        active_all = s.query(Mantra).filter(Mantra.superseded_by == 0).all()
        pool = [m for m in active_all if m.category == category]
        active_count = sum(1 for m in active_all if m.enabled)
        if active_count >= cfg.max_active_mantras:
            return {
                "action": "rejected",
                "reason": f"生效心法已达上限 {cfg.max_active_mantras}，请先归档低分条目",
                "title": title,
            }

        def _bump_version() -> None:
            """同一事务内推进心法库版本，供乐观并发校验（CAS）."""
            from ..models import EngineState

            row = s.get(EngineState, "mantra_version")
            token = str(int(time.time() * 1000))
            if row is None:
                s.add(EngineState(key="mantra_version", value=token))
            else:
                row.value = token

        def _done(payload: dict[str, Any]) -> dict[str, Any]:
            _bump_version()
            return payload

        def _create(version: int) -> Mantra:
            new = Mantra(
                title=title, rule=rule, tags=tags or [], category=category,
                source=source, confidence=confidence, health=confidence,
                enabled=True, version=max(1, version),
                evidence=evidence or {}, created_at=now, updated_at=now, expires_at=expires,
            )
            s.add(new)
            s.flush()
            return new

        def _replace(old: Mantra, sim: float, why: str) -> dict[str, Any]:
            new = _create(old.version + 1)
            old.enabled = False
            old.superseded_by = new.id
            old.updated_at = now
            old.evidence = {
                **(old.evidence or {}),
                "superseded_reason": why[:200],
                "superseded_by_title": title[:80],
            }
            log.info("心法替代: #%s v%s -> #%s v%s", old.id, old.version, new.id, new.version)
            return _done({
                "action": "replaced",
                "mantra": serialize(new, cfg.ttl_days),
                "replaced_id": old.id,
                "reason": f"{why}（相似度 {sim:.2f}）",
            })

        # 1) AI 点名替代：仍需过冷却/置信度门槛，模型一句话不能直接换掉既有心法
        if replaces:
            target: Mantra | None = None
            target_sim = 0.0
            for m in active_all:
                sim = similarity(m.title, replaces)
                if sim > target_sim:
                    target, target_sim = m, sim
            if target is not None and target_sim >= 0.45 and _can_replace(target, confidence, rule, now):
                return _replace(target, target_sim, "AI 判定语义重复且通过替代门槛")

        # 2) 自动判定：标题 + 规则加权相似度
        best: Mantra | None = None
        best_sim = 0.0
        for m in pool:
            sim = lesson_similarity(m.title, m.rule, title, rule)
            if sim > best_sim:
                best, best_sim = m, sim

        if best is not None and best_sim >= REPLACE_SIM and _can_replace(best, confidence, rule, now):
            return _replace(best, best_sim, "新版本与旧心法语义重合且更优")

        if best is not None and best_sim >= MERGE_SIM:
            best.hits += 1
            best.updated_at = now
            best.expires_at = expires
            best.evidence = {
                **(best.evidence or {}),
                "last_evidence": evidence or {},
                "reinforced": (best.evidence or {}).get("reinforced", 0) + 1,
            }
            return _done({
                "action": "merged",
                "mantra": serialize(best, cfg.ttl_days),
                "reason": f"与旧心法相似度 {best_sim:.2f}, 合并强化（冷却期内不替代）",
            })

        new = _create(1)
        return _done({"action": "created", "mantra": serialize(new, cfg.ttl_days), "reason": "新心法入库"})


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
            # 不再用单笔胜负微调模型自报的 confidence：
            # 质量由 refresh_health_from_evidence 按整组平仓证据重算，避免噪声污染分数


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


def dedupe() -> dict[str, Any]:
    """一次性整理存量心法：语义重复的只保留置信度最高的一条，其余标记为被替代.

    新复盘已经会自动替代，但修复前积累的重复条目需要一次清理。
    """
    cfg = get_evolution()
    now = int(time.time())
    replaced: list[dict[str, Any]] = []
    with session_scope() as s:
        rows = (
            s.query(Mantra)
            .filter(Mantra.enabled.is_(True), Mantra.superseded_by == 0)
            .order_by(Mantra.confidence.desc(), Mantra.created_at.desc())
            .all()
        )
        kept: list[Mantra] = []
        for m in rows:
            match = next(
                (
                    k for k in kept
                    if lesson_similarity(k.title, k.rule, m.title, m.rule) >= REPLACE_SIM
                ),
                None,
            )
            if match is None:
                kept.append(m)
                continue
            m.enabled = False
            m.superseded_by = match.id
            m.updated_at = now
            m.evidence = {
                **(m.evidence or {}),
                "superseded_reason": "存量去重：与更高置信度心法语义重复",
                "superseded_by_title": match.title[:80],
            }
            replaced.append({"id": m.id, "title": m.title, "kept_id": match.id, "kept_title": match.title})
    return {"ok": True, "kept": len(kept), "replaced": replaced, "total": len(rows)}


def purge_superseded() -> int:
    """归档被替代的旧版本（保留审计链，**绝不物理删除**）.

    名称沿用旧接口，语义已改为：把 ``superseded_by != 0`` 且仍启用/未留痕的条目
    标记为停用归档并写入 ``archived_reason``。手动停用（superseded_by=0）不受影响；
    重复调用幂等。管理员仍可通过 DELETE 接口显式删除。
    """
    now = int(time.time())
    archived = 0
    with session_scope() as s:
        rows = s.query(Mantra).filter(Mantra.superseded_by != 0).all()
        for row in rows:
            evidence = dict(row.evidence or {})
            if evidence.get("archived_reason") and not row.enabled:
                continue
            row.enabled = False
            row.updated_at = now
            evidence.setdefault("archived_reason", "被新版本替代，归档停用留痕（可人工复核恢复）")
            evidence["archived_at"] = now
            row.evidence = evidence
            archived += 1
    if archived:
        log.info("归档被替代心法 %d 条（保留记录）", archived)
    return archived


def refresh_health_from_evidence(limit: int = 500) -> dict[str, Any]:
    """按最近平仓证据重算全部心法健康分，并按需归档。走 CAS，失败只记日志.

    每轮批量复盘结束后调用一次：这是把 health 从"装饰品"变成承重件的落地入口，
    也是效果评估反馈回生成的通道（低于对照组的条目写入 evidence.effectiveness_verdict）。
    """
    from ..models import TradeRecord
    from ..services import state
    from . import effectiveness, health

    cfg = get_evolution()
    version = state.get("mantra_version", "0")
    try:
        with session_scope() as s:
            rows = s.query(Mantra).all()
            closed = (
                s.query(TradeRecord).order_by(TradeRecord.closed_at.desc()).limit(limit).all()
            )
            # 乐观并发：复盘期间若管理员/其它进程已改动心法库，放弃本轮重算
            from ..models import EngineState

            vrow = s.get(EngineState, "mantra_version")
            if (vrow.value if vrow else "0") != version:
                return {"status": "conflict", "changes": []}
            trade_rows = health.trade_rows(closed)
            lessons = [serialize(r, cfg.ttl_days) for r in rows]
            updated, changes = health.refresh_lesson_health(
                lessons,
                closed_trades=trade_rows,
                min_sample=cfg.min_health_sample,
                threshold=cfg.archive_health_threshold,
            )
            by_id = {int(x["id"]): x for x in updated}

            # 观察性对照反馈：注入组明显更差的逐条心法标记为 underperform（影响注入排序）
            try:
                eff = effectiveness.evaluate(limit=limit)
                control = eff.get("control") or {}
                control_avg = float(control.get("avg_pnl") or 0)
                for item in eff.get("per_mantra") or []:
                    mid = int(item.get("id") or 0)
                    if mid not in by_id or int(item.get("trades") or 0) < effectiveness.MIN_SAMPLE:
                        continue
                    if float(item.get("avg_pnl") or 0) < control_avg:
                        for row in rows:
                            if row.id == mid:
                                ev = dict(row.evidence or {})
                                ev["effectiveness_verdict"] = "underperform"
                                row.evidence = ev
                                break
            except Exception as exc:  # noqa: BLE001  评估是增强，不能打断复盘
                log.warning("效果反馈跳过: %s", exc)

            for row in rows:
                fresh = by_id.get(row.id)
                if not fresh or not fresh.get("health_scored"):
                    continue
                row.health = float(fresh["health"])
                row.enabled = bool(fresh["enabled"])
                row.updated_at = int(time.time())
                ev = dict(row.evidence or {})
                ev["health_basis"] = (fresh.get("evidence") or {}).get("health_basis", {})
                if (fresh.get("evidence") or {}).get("archived_reason"):
                    ev["archived_reason"] = fresh["evidence"]["archived_reason"]
                row.evidence = ev
            if changes:
                vrow = s.get(EngineState, "mantra_version")
                token = str(int(time.time() * 1000))
                if vrow is None:
                    s.add(EngineState(key="mantra_version", value=token))
                else:
                    vrow.value = token
        return {
            "status": "ok" if changes else "unchanged",
            "rescored": sum(1 for c in changes if c["action"] == "rescore"),
            "archived": sum(1 for c in changes if c["action"] == "archive"),
            "changes": changes[:50],
        }
    except Exception as exc:  # noqa: BLE001
        log.warning("心法健康度重算跳过（不影响复盘）: %s", exc)
        return {"status": "error", "reason": str(exc)[:200], "changes": []}


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
            "archived": sum(1 for r in rows if "archived_reason" in (r.evidence or {})),
            "evidence_scored": sum(1 for r in rows if "health_basis" in (r.evidence or {})),
            "by_category": by_cat,
            "avg_confidence": round(avg_conf, 3),
            "ttl_days": cfg.ttl_days,
            "max_injected": cfg.max_injected,
            "min_lesson_sample": cfg.min_lesson_sample,
            "batch_review_size": cfg.batch_review_size,
            "min_health_sample": cfg.min_health_sample,
        }
