"""场景化经验库: 打标结论归档 + 相似场景 Top-k 检索注入 (RAG).

检索不依赖 embedding，用「同币种 + 标签 Jaccard + 数值场景相似度」的确定性打分，
零 token、可解释、可回归测试。经验只增不减到上限后归档最旧条目（停用不删除）。
"""
from __future__ import annotations

import time
from typing import Any

from sqlalchemy.orm import Session

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import DecisionJournal, Experience
from ..services.settings import get_journal

log = get_logger("journal.experience")

#: 数值场景的容差跨度（用于归一化相似度，越大越宽松）
_SPREAD = {"rsi": 70.0, "adx": 60.0, "atr_pct": 5.0, "funding": 0.5}

#: 经验正文的紧凑符号：固定装饰文字 → 特殊符号（压缩注入 token，语义自解释）
_HEAD = "◈"       # 「历史经验」区段标记
_HINT = "≈"       # 「场景化检索，仅概率提示，不违风控铁律」
_WIN = "✓"        # 判断正确 / 胜
_LOSS = "✗"       # 判断错误 / 负
_LONG = "L"       # 做多
_SHORT = "S"      # 做空
_DELTA = "Δ"      # 实际方向收益
_TARGET = "→"     # 目标收益
_CONF = "c"       # 信心


def _num(v: Any) -> float | None:
    if v is None or v == "":
        return None
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def derive_scenario(snapshot: dict[str, Any]) -> dict[str, float]:
    out: dict[str, float] = {}
    for key in _SPREAD:
        val = _num((snapshot or {}).get(key))
        if val is not None:
            out[key] = val
    return out


def derive_tags(symbol: str, snapshot: dict[str, Any], direction: str = "") -> list[str]:
    tags: list[str] = [f"sym:{symbol}"]
    rsi = _num((snapshot or {}).get("rsi"))
    adx = _num((snapshot or {}).get("adx"))
    if rsi is not None:
        if rsi >= 70:
            tags.append("rsi_overbought")
        elif rsi <= 30:
            tags.append("rsi_oversold")
        else:
            tags.append("rsi_neutral")
    if adx is not None:
        tags.append("adx_trend" if adx >= 25 else "adx_chop")
    if direction:
        tags.append(f"dir:{direction}")
    return tags


def _jaccard(a: set[str], b: set[str]) -> float:
    if not a or not b:
        return 0.0
    return len(a & b) / len(a | b)


def _similarity(query_symbol: str, query_scenario: dict, query_tags: list[str], exp: Experience) -> float:
    score = 0.0
    if exp.symbol == query_symbol:
        score += 0.35
    score += 0.45 * _jaccard(set(query_tags or []), set(exp.tags or []))
    exp_scenario = exp.scenario if isinstance(exp.scenario, dict) else {}
    num_score = 0.0
    count = 0
    for key, spread in _SPREAD.items():
        a = _num(query_scenario.get(key))
        b = _num(exp_scenario.get(key))
        if a is not None and b is not None:
            num_score += 1.0 - min(1.0, abs(a - b) / spread)
            count += 1
    if count:
        score += 0.2 * (num_score / count)
    return round(min(1.0, score), 4)


def serialize(r: Experience) -> dict[str, Any]:
    return {
        "id": r.id,
        "symbol": r.symbol,
        "tags": r.tags or [],
        "scenario": r.scenario or {},
        "lesson": r.lesson,
        "outcome": r.outcome,
        "source_decision_id": r.source_decision_id,
        "source": r.source,
        "hits": r.hits,
        "wins": r.wins,
        "losses": r.losses,
        "enabled": r.enabled,
        "created_at": r.created_at,
        "updated_at": r.updated_at,
    }


def _lesson_text(row: DecisionJournal) -> str:
    """经验正文（紧凑符号版）：固定装饰用符号代替，变量值保留。

    形如 `ETH|超卖|震荡 L✗ Δ-2.00% →+3.00% c0.72`，符号含义见 injection 头部图例。
    """
    snap = row.snapshot if isinstance(row.snapshot, dict) else {}
    rsi = _num(snap.get("rsi"))
    adx = _num(snap.get("adx"))
    scene: list[str] = []
    if rsi is not None:
        scene.append("超卖" if rsi <= 30 else ("超买" if rsi >= 70 else "中性"))
    if adx is not None:
        scene.append("趋势" if adx >= 25 else "震荡")
    ctx = ("|" + "|".join(scene)) if scene else ""
    direction = _LONG if row.direction == "long" else _SHORT
    verdict = _WIN if row.outcome == "correct" else _LOSS
    return (
        f"{row.symbol}{ctx} {direction}{verdict} "
        f"{_DELTA}{row.actual_pct:+.2f}% {_TARGET}{row.target_pct:+.2f}% {_CONF}{row.confidence:.2f}"
    )


def _archive_one(s: Session, row: DecisionJournal) -> str:
    """把一条已打标的决策归档为经验；返回 created / merged."""
    snap = row.snapshot if isinstance(row.snapshot, dict) else {}
    tags = derive_tags(row.symbol, snap, row.direction)
    outcome = "win" if row.outcome == "correct" else "loss"
    scenario = derive_scenario(snap)

    # 同 symbol + 同 outcome + 标签集合完全一致 → 合并强化（累计命中），不反复新建
    dup = (
        s.query(Experience)
        .filter(
            Experience.enabled.is_(True),
            Experience.symbol == row.symbol,
            Experience.outcome == outcome,
        )
        .all()
    )
    tag_set = set(tags)
    for e in dup:
        if set(e.tags or []) == tag_set:
            e.hits = int(e.hits or 0) + 1
            if outcome == "win":
                e.wins = int(e.wins or 0) + 1
            else:
                e.losses = int(e.losses or 0) + 1
            e.updated_at = int(time.time())
            return "merged"

    s.add(
        Experience(
            symbol=row.symbol,
            tags=tags,
            scenario=scenario,
            lesson=_lesson_text(row),
            outcome=outcome,
            source_decision_id=row.id,
            source="journal",
            hits=1,
            wins=1 if outcome == "win" else 0,
            losses=0 if outcome == "win" else 1,
            enabled=True,
            created_at=int(time.time()),
            updated_at=int(time.time()),
        )
    )
    # 同一批内先建的经验必须立即可见，否则后续决策的合并去重查不到它
    s.flush()
    return "created"


def _prune(s: Session) -> int:
    """超出容量上限时归档最旧条目（停用不删除），返回归档数."""
    cfg = get_journal()
    enabled = (
        s.query(Experience)
        .filter(Experience.enabled.is_(True))
        .order_by(Experience.created_at.asc())
        .all()
    )
    overflow = len(enabled) - cfg.max_experiences
    if overflow <= 0:
        return 0
    for e in enabled[:overflow]:
        e.enabled = False
        e.updated_at = int(time.time())
    return overflow


def archive_labeled(limit: int = 200) -> dict[str, Any]:
    """把已打标且未归档的决策沉淀为经验（correct→win / wrong→loss，partial 不归档）。"""
    cfg = get_journal()
    if not cfg.enabled:
        return {"ok": False, "error": "决策日志已关闭"}
    created = merged = 0
    with session_scope() as s:
        rows = (
            s.query(DecisionJournal)
            .filter(
                DecisionJournal.status == "labeled",
                DecisionJournal.archived.is_(False),
                DecisionJournal.outcome.in_(["correct", "wrong"]),
            )
            .order_by(DecisionJournal.labeled_at.asc())
            .limit(limit)
            .all()
        )
        for row in rows:
            if _archive_one(s, row) == "created":
                created += 1
            else:
                merged += 1
            row.archived = True
        pruned = _prune(s)
    return {"ok": True, "archived": created, "merged": merged, "pruned": pruned}


def retrieve_top_k(
    symbol: str,
    scenario: dict[str, Any] | None = None,
    tags: list[str] | None = None,
    limit: int | None = None,
) -> list[dict[str, Any]]:
    """按相似场景检索 Top-k 条经验（确定性打分，无 embedding）。"""
    cfg = get_journal()
    limit = limit if limit is not None else cfg.retrieve_top_k
    if limit <= 0:
        return []
    with session_scope() as s:
        rows = s.query(Experience).filter(Experience.enabled.is_(True)).all()
    scored: list[tuple[float, Experience]] = []
    for r in rows:
        sc = _similarity(symbol, scenario or {}, tags or [], r)
        if sc < cfg.min_similarity:
            continue
        scored.append((sc, r))
    scored.sort(key=lambda x: (-x[0], x[1].id))
    return [serialize(r) for _, r in scored[:limit]]


def injection(symbol: str, snapshot: dict[str, Any]) -> tuple[str, list[int], list[dict[str, Any]]]:
    """返回 (注入文本, 注入 id 列表, 明细)，供 AI 决策上下文使用。"""
    cfg = get_journal()
    if not cfg.enabled or not cfg.inject_experiences or cfg.retrieve_top_k <= 0:
        return "", [], []
    tags = derive_tags(symbol, snapshot)
    scenario = derive_scenario(snapshot)
    items = retrieve_top_k(symbol, scenario, tags, cfg.retrieve_top_k)
    if not items:
        return "", [], []
    # 头部：固定装饰符号化 + 一行符号图例（自解释，避免每个字段重复中文）
    lines = [
        f"{_HEAD}{_HINT}[{_WIN}对 {_LOSS}错 {_LONG}多 {_SHORT}空 "
        f"{_DELTA}实 {_TARGET}标 {_CONF}信]"
    ]
    for i, m in enumerate(items, 1):
        lines.append(f"{i}.{m['lesson']}")
    return "\n".join(lines), [m["id"] for m in items], items


def list_experiences(limit: int = 300, symbol: str = "") -> list[dict[str, Any]]:
    with session_scope() as s:
        q = s.query(Experience).order_by(Experience.created_at.desc())
        if symbol:
            q = q.filter(Experience.symbol == symbol)
        rows = q.limit(limit).all()
        return [serialize(r) for r in rows]


def experience_stats() -> dict[str, Any]:
    cfg = get_journal()
    with session_scope() as s:
        rows = s.query(Experience).all()
        total = len(rows)
        enabled = sum(1 for r in rows if r.enabled)
        wins = sum(1 for r in rows if r.outcome == "win")
    return {
        "total": total,
        "enabled": enabled,
        "disabled": total - enabled,
        "wins": wins,
        "losses": total - wins,
        "max": cfg.max_experiences,
        "retrieve_top_k": cfg.retrieve_top_k,
    }


def delete_experience(experience_id: int) -> bool:
    with session_scope() as s:
        row = s.get(Experience, experience_id)
        if not row:
            return False
        s.delete(row)
        return True
