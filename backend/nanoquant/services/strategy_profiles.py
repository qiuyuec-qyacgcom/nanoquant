"""AI 分析策略：把用户显式配置的交易策略注入 AI 决策上下文.

定位澄清（2026-11 修正）：
- 策略**不筛选标的池**，引擎仍全量轮询标的池；
- 每个标的决策前，按 ``priority`` 找出**适用范围匹配**的第一条启用策略，
  把它的 ``rule`` 交给 AI，让模型按这条策略分析是否开仓、方向与节奏；
- 一条策略都没启用（或该标的不在适用范围）⇒ ``strategy=None``，
  AI 按通用铁律分析，即「未添加策略则默认」。
"""
from __future__ import annotations

from typing import Any

from ..db import session_scope
from ..models import StrategyProfile

SIDES = ("any", "long", "short")
MAX_RULE_CHARS = 2000


class StrategyError(ValueError):
    """策略校验失败（路由层转 400）."""

    def __init__(self, detail: str) -> None:
        super().__init__(detail)
        self.detail = detail


def normalize_filters(raw: Any) -> dict[str, Any]:
    """适用范围：标的白名单 + 方向偏好（其余旧字段忽略，保持向后兼容）."""
    src = raw if isinstance(raw, dict) else {}
    symbols = src.get("symbols") or []
    if isinstance(symbols, str):
        symbols = symbols.replace("，", ",").split(",")
    symbols = sorted({str(s).strip().upper() for s in symbols if str(s).strip()})
    side = str(src.get("side") or "any").strip().lower()
    if side not in SIDES:
        side = "any"
    return {"symbols": symbols, "side": side}


def normalize_rule(text: Any) -> str:
    rule = str(text or "").strip()
    if len(rule) > MAX_RULE_CHARS:
        rule = rule[:MAX_RULE_CHARS]
    return rule


def summarize(filters: dict[str, Any]) -> str:
    f = normalize_filters(filters)
    scope = "/".join(f["symbols"][:4]) + ("…" if len(f["symbols"]) > 4 else "") if f["symbols"] else "全池"
    side = {"long": "只做多", "short": "只做空"}.get(f["side"], "不限方向")
    return f"{scope} · {side}"


def serialize(row: StrategyProfile) -> dict[str, Any]:
    filters = normalize_filters(row.filters)
    return {
        "id": row.id,
        "name": row.name,
        "description": row.description or "",
        "rule": row.rule or "",
        "enabled": bool(row.enabled),
        "priority": int(row.priority or 100),
        "filters": filters,
        "summary": summarize(filters),
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    }


def _order(query):
    return query.order_by(StrategyProfile.priority.asc(), StrategyProfile.id.asc())


def list_profiles() -> list[dict[str, Any]]:
    with session_scope() as s:
        return [serialize(r) for r in _order(s.query(StrategyProfile)).all()]


def active_profiles() -> list[dict[str, Any]]:
    """启用的策略（按优先级）；空列表 = 全部走默认分析."""
    with session_scope() as s:
        rows = _order(
            s.query(StrategyProfile).filter(StrategyProfile.enabled.is_(True))
        ).all()
        return [serialize(r) for r in rows]


def stats() -> dict[str, Any]:
    with session_scope() as s:
        rows = s.query(StrategyProfile).all()
        enabled = [r for r in rows if r.enabled]
        return {
            "total": len(rows),
            "enabled": len(enabled),
            "default": len(enabled) == 0,
            "by_side": {
                side: sum(1 for r in enabled if normalize_filters(r.filters)["side"] == side)
                for side in SIDES
            },
        }


def create_profile(
    *,
    name: str,
    description: str = "",
    rule: str = "",
    enabled: bool = True,
    priority: int = 100,
    filters: Any = None,
) -> dict[str, Any]:
    name = (name or "").strip()
    if len(name) < 2:
        raise StrategyError("策略名称至少 2 个字")
    body = normalize_rule(rule)
    if len(body) < 8:
        raise StrategyError("策略内容至少 8 个字（AI 需要可执行的判断依据）")
    payload = normalize_filters(filters)
    with session_scope() as s:
        if s.query(StrategyProfile).filter(StrategyProfile.name == name).first():
            raise StrategyError(f"同名策略已存在: {name}")
        row = StrategyProfile(
            name=name,
            description=(description or "")[:200],
            rule=body,
            enabled=bool(enabled),
            priority=int(priority),
            filters=payload,
        )
        s.add(row)
        s.flush()
        return serialize(row)


def update_profile(profile_id: int, patch: dict[str, Any]) -> dict[str, Any] | None:
    with session_scope() as s:
        row = s.get(StrategyProfile, profile_id)
        if row is None:
            return None
        if "name" in patch and patch["name"] is not None:
            name = str(patch["name"]).strip()
            if len(name) < 2:
                raise StrategyError("策略名称至少 2 个字")
            dup = (
                s.query(StrategyProfile)
                .filter(StrategyProfile.name == name, StrategyProfile.id != profile_id)
                .first()
            )
            if dup:
                raise StrategyError(f"同名策略已存在: {name}")
            row.name = name
        if "description" in patch and patch["description"] is not None:
            row.description = str(patch["description"])[:200]
        if "rule" in patch and patch["rule"] is not None:
            body = normalize_rule(patch["rule"])
            if len(body) < 8:
                raise StrategyError("策略内容至少 8 个字（AI 需要可执行的判断依据）")
            row.rule = body
        if "enabled" in patch and patch["enabled"] is not None:
            row.enabled = bool(patch["enabled"])
        if "priority" in patch and patch["priority"] is not None:
            row.priority = int(patch["priority"])
        if "filters" in patch and patch["filters"] is not None:
            row.filters = normalize_filters(patch["filters"])
        s.flush()
        return serialize(row)


def delete_profile(profile_id: int) -> bool:
    with session_scope() as s:
        row = s.get(StrategyProfile, profile_id)
        if row is None:
            return False
        s.delete(row)
        return True


# ------------------------------------------------------------------ 纯匹配逻辑
def matches_symbol(symbol: str, profile: dict[str, Any]) -> bool:
    """该策略是否适用于这个标的（只按 symbols 白名单，方向偏好不影响适用范围）."""
    if not profile.get("enabled", True):
        return False
    scope = normalize_filters(profile.get("filters"))["symbols"]
    return not scope or symbol.upper() in scope


def pick_from(symbol: str, profiles: list[dict[str, Any]] | None) -> dict[str, Any] | None:
    """按优先级取第一条适用的策略；None 表示该标的走默认分析."""
    ordered = sorted(
        [p for p in (profiles or []) if p.get("enabled", True)],
        key=lambda p: (int(p.get("priority") or 100), int(p.get("id") or 0)),
    )
    return next((p for p in ordered if matches_symbol(symbol, p)), None)


def pick_for(symbol: str) -> dict[str, Any] | None:
    """单标的取策略（便捷入口，内部查一次库；引擎批量场景请用 pick_from）."""
    return pick_from(symbol, active_profiles())
